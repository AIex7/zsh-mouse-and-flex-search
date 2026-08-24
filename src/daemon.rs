use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use rusqlite::OptionalExtension;

use crate::db::*;
use crate::layout::shell_words_for_matching;
use crate::protocol::*;
use crate::search::{normalize_matching_words, CandidateInput, NativeHistory};

type CustomHistoryRefreshRow = (i64, String, String, String, i64);

pub(crate) struct CustomHistoryRefreshSnapshot {
    pub(crate) rows: Vec<CustomHistoryRefreshRow>,
    pub(crate) status_rows: Vec<(i64, usize)>,
    pub(crate) revision: Option<i64>,
}

pub(crate) fn read_custom_history_refresh_snapshot_with_hook<F>(
    conn: &mut rusqlite::Connection,
    watermark_row_id: i64,
    status_revision: i64,
    after_rows_read: F,
) -> Result<CustomHistoryRefreshSnapshot, rusqlite::Error>
where
    F: FnOnce(),
{
    let tx = conn.transaction()?;

    let rows = {
        let mut stmt = tx.prepare(
            "SELECT id, command, cwd, timestamp, failed FROM custom_history WHERE id >= ? ORDER BY id DESC",
        )?;
        let mapped = stmt.query_map([watermark_row_id], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2).unwrap_or_default(),
                row.get(3).unwrap_or_default(),
                row.get(4).unwrap_or(0),
            ))
        })?;
        mapped.collect::<Result<Vec<_>, _>>()?
    };

    // Tests use this hook to commit a concurrent writer after SQLite has fixed
    // the read snapshot. Production passes a no-op closure.
    after_rows_read();

    let status_rows = {
        let mut stmt = tx.prepare(
            "SELECT h.failed, (SELECT COUNT(*) FROM custom_history AS newer WHERE newer.id > h.id)
             FROM custom_history AS h
             WHERE h.status_revision > ?
             ORDER BY h.status_revision",
        )?;
        let mapped = stmt.query_map([status_revision], |row| {
            Ok((row.get(0)?, row.get::<_, i64>(1)? as usize))
        })?;
        mapped.collect::<Result<Vec<_>, _>>()?
    };

    let revision = tx
        .query_row(
            "SELECT status_revision FROM custom_history_metadata WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .optional()?;

    tx.commit()?;
    Ok(CustomHistoryRefreshSnapshot {
        rows,
        status_rows,
        revision,
    })
}

pub fn history_file_signature(path: &Path) -> (i64, i64, u64) {
    if let Ok(meta) = fs::metadata(path) {
        use std::os::unix::fs::MetadataExt;
        (meta.mtime(), meta.mtime_nsec(), meta.size())
    } else {
        (0, 0, 0)
    }
}

pub fn daemon_debug_log(enabled: bool, message: &str) {
    if enabled {
        eprintln!("[zsh_flex_history daemon] {}", message);
    }
}

pub struct DaemonHistoryState {
    pub path: PathBuf,
    pub use_custom_history: bool,
    pub history_length: Option<usize>,
    pub native_candidates: NativeHistory,
    pub custom_history_watermark: Option<CustomHistoryWatermark>,
    pub custom_history_status_revision: i64,
}

impl DaemonHistoryState {
    pub fn load(
        path: &Path,
        use_custom_history: bool,
        history_length: Option<usize>,
    ) -> Self {
        if use_custom_history {
            let (native_candidates, watermark, status_revision) =
                build_native_custom_history_candidates(path, history_length);
            Self {
                path: path.to_path_buf(),
                use_custom_history,
                history_length,
                native_candidates,
                custom_history_watermark: watermark,
                custom_history_status_revision: status_revision,
            }
        } else {
            let history = load_plain_zsh_history(path);
            let native_candidates = NativeHistory::new(history);
            Self {
                path: path.to_path_buf(),
                use_custom_history,
                history_length,
                native_candidates,
                custom_history_watermark: None,
                custom_history_status_revision: 0,
            }
        }
    }

    pub fn len(&self) -> usize {
        self.native_candidates.len()
    }

    pub fn is_empty(&self) -> bool {
        self.native_candidates.is_empty()
    }

    fn rebuild_native(&mut self) {
        if self.use_custom_history {
            let (native_candidates, watermark, status_revision) =
                build_native_custom_history_candidates(&self.path, self.history_length);
            self.native_candidates = native_candidates;
            self.custom_history_watermark = watermark;
            self.custom_history_status_revision = status_revision;
        } else {
            let history = load_plain_zsh_history(&self.path);
            self.native_candidates = NativeHistory::new(history);
            self.custom_history_watermark = None;
            self.custom_history_status_revision = 0;
        }
    }

    pub fn refresh(&mut self) {
        self.native_candidates.clear_daemon_query_cache();
        if !self.use_custom_history {
            self.rebuild_native();
            return;
        }

        let watermark = match &self.custom_history_watermark {
            Some(w) => w.clone(),
            None => {
                self.rebuild_native();
                return;
            }
        };

        let mut conn = match rusqlite::Connection::open(&self.path) {
            Ok(c) => c,
            Err(_) => {
                self.rebuild_native();
                return;
            }
        };

        let snapshot = match read_custom_history_refresh_snapshot_with_hook(
            &mut conn,
            watermark.row_id,
            self.custom_history_status_revision,
            || {},
        ) {
            Ok(snapshot) => snapshot,
            Err(_) => {
                self.rebuild_native();
                return;
            }
        };
        let rows = snapshot.rows;

        let mut anchor = None;
        let mut watermark_replaced = false;
        let mut new_records = Vec::new();

        for r in &rows {
            if r.0 == watermark.row_id {
                anchor = Some(r.clone());
            }
            if r.0 > watermark.row_id {
                if r.1 == watermark.command && Some(&r.2) == watermark.cwd.as_ref() {
                    watermark_replaced = true;
                }
                if let Some(candidate) = make_candidate_input(r.1.clone(), Some(r.2.clone()), r.4 != 0) {
                    new_records.push((r.0, candidate));
                }
            }
        }

        if (anchor.is_none() && !watermark_replaced)
            || (anchor.is_some()
                && anchor.as_ref().map(|a| (&a.1, a.2.as_str()))
                    != Some((&watermark.command, watermark.cwd.as_deref().unwrap_or(""))))
        {
            self.rebuild_native();
            return;
        }

        if !new_records.is_empty() {
            let changed_entries: Vec<CandidateInput> = new_records.iter().map(|nr| nr.1.clone()).collect();
            self.native_candidates.prepend_replacing(changed_entries);
            if let Some(limit) = self.history_length {
                self.native_candidates.truncate(limit);
            }
        }

        for (failed, candidate_index) in snapshot.status_rows {
            if let Some(limit) = self.history_length {
                if candidate_index >= limit {
                    continue;
                }
            }
            self.native_candidates.update_failed_at(candidate_index, failed != 0);
        }

        if let Some(first_rec) = rows.first() {
            self.custom_history_watermark = Some(CustomHistoryWatermark {
                row_id: first_rec.0,
                command: first_rec.1.clone(),
                cwd: if first_rec.2.is_empty() { None } else { Some(normalize_cwd_value(&first_rec.2)) },
                timestamp: if first_rec.3.is_empty() { None } else { Some(first_rec.3.clone()) },
            });
        }
        if let Some(rev) = snapshot.revision {
            self.custom_history_status_revision = rev;
        }
    }

    pub fn search_response(
        &self,
        query: &str,
        candidate_indices: Option<&[usize]>,
        limit: Option<usize>,
        current_cwd: Option<&str>,
    ) -> Result<Vec<u8>, &'static str> {
        let normalized_query = query.trim().to_lowercase();
        let prefix_words = normalize_matching_words(shell_words_for_matching(query));
        let ordered_words = normalize_matching_words(
            query
                .to_lowercase()
                .split_whitespace()
                .map(str::to_string)
                .collect(),
        );
        let cwd_arc = current_cwd.and_then(|cwd| self.native_candidates.cwd_interner.get(cwd).cloned());

        if candidate_indices.is_some() {
            self.native_candidates.search_response_frame(
                &query.to_lowercase(),
                &normalized_query,
                &prefix_words,
                &ordered_words,
                cwd_arc.as_ref(),
                candidate_indices,
                limit,
                10_000,
            )
        } else {
            self.native_candidates.search_response_frame_for_daemon(
                &query.to_lowercase(),
                &normalized_query,
                &prefix_words,
                &ordered_words,
                cwd_arc.as_ref(),
                None,
                limit,
            )
        }
    }
}

pub struct HistoryDaemonClient {
    pub socket_path: PathBuf,
    pub history_path: PathBuf,
    pub debug: bool,
    pub history_length: Option<usize>,
    pub use_custom_history: bool,
}

impl HistoryDaemonClient {
    pub fn ping(&self, timeout: Duration) -> bool {
        let request = serialize_ping_request_bytes();
        let response = daemon_exchange_bytes(&self.socket_path.to_string_lossy(), &request, timeout);
        response.is_some_and(|resp| {
            FrameReader::new(&resp, FRAME_PONG_RESPONSE).is_some_and(|r| r.done())
        })
    }

    pub fn ensure_running(&self) -> bool {
        if self.ping(Duration::from_millis(150)) {
            daemon_debug_log(self.debug, &format!("using existing daemon at {}", self.socket_path.display()));
            return true;
        }

        if self.socket_path.exists() {
            let _ = fs::remove_file(&self.socket_path);
            daemon_debug_log(self.debug, &format!("removed stale socket at {}", self.socket_path.display()));
        }

        daemon_debug_log(self.debug, &format!("starting new daemon at {}", self.socket_path.display()));
        if !launch_history_daemon(&self.history_path, &self.socket_path, self.history_length, self.use_custom_history) {
            daemon_debug_log(self.debug, "failed to launch daemon process");
            return false;
        }

        let deadline = Instant::now() + Duration::from_millis(1000);
        while Instant::now() < deadline {
            if self.ping(Duration::from_millis(150)) {
                daemon_debug_log(self.debug, "new daemon is ready");
                return true;
            }
            std::thread::sleep(Duration::from_millis(30));
        }
        daemon_debug_log(self.debug, "daemon did not become ready before timeout");
        false
    }

    pub fn search_history(
        &self,
        query: &str,
        candidate_indices: Option<&[usize]>,
        limit: Option<i64>,
        cwd: Option<&str>,
    ) -> Option<ParsedResponse> {
        let req_bytes = serialize_search_request_bytes(query, candidate_indices, limit, cwd).ok()?;
        let resp_bytes = daemon_exchange_bytes(&self.socket_path.to_string_lossy(), &req_bytes, Duration::from_millis(500));
        let resp_bytes = match resp_bytes {
            Some(b) => b,
            None => {
                if !self.ensure_running() {
                    return None;
                }
                daemon_exchange_bytes(&self.socket_path.to_string_lossy(), &req_bytes, Duration::from_millis(500))?
            }
        };
        parse_search_response_bytes(&resp_bytes)
    }
}

pub fn launch_history_daemon(
    history_path: &Path,
    socket_path: &Path,
    history_length: Option<usize>,
    use_custom_history: bool,
) -> bool {
    if let Some(parent) = socket_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let current_exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(_) => return false,
    };

    let mut cmd = Command::new(current_exe);
    cmd.arg("--daemon")
        .arg("--history-file")
        .arg(history_path)
        .arg("--socket-path")
        .arg(socket_path);

    if let Some(len) = history_length {
        cmd.arg("--history-length").arg(len.to_string());
    }
    if use_custom_history {
        cmd.arg("--use-custom-history");
    }

    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    detach_daemon_process(&mut cmd);

    cmd.spawn().is_ok()
}

pub(crate) fn detach_daemon_process(cmd: &mut Command) {
    // SAFETY: setsid is async-signal-safe and the closure does not allocate or
    // access shared state between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
}

pub fn run_history_daemon(
    history_path: &Path,
    socket_path: &Path,
    debug: bool,
    history_length: Option<usize>,
    use_custom_history: bool,
) -> i32 {
    if use_custom_history && !history_path.exists() {
        let _ = ensure_custom_history_file(history_path);
    }
    let mut history_state = DaemonHistoryState::load(history_path, use_custom_history, history_length);
    let mut signature = history_file_signature(history_path);

    if let Some(parent) = socket_path.parent() {
        let _ = fs::create_dir_all(parent);
    }

    if socket_path.exists() {
        let client = HistoryDaemonClient {
            socket_path: socket_path.to_path_buf(),
            history_path: history_path.to_path_buf(),
            debug,
            history_length,
            use_custom_history,
        };
        if client.ping(Duration::from_millis(150)) {
            daemon_debug_log(debug, &format!("daemon already running at {}, exiting", socket_path.display()));
            return 0;
        }
        let _ = fs::remove_file(socket_path);
        daemon_debug_log(debug, &format!("removed stale socket at {}", socket_path.display()));
    }

    let listener = match UnixListener::bind(socket_path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("zsh_flex_history daemon: failed to bind socket: {}", e);
            return 1;
        }
    };
    let _ = fs::set_permissions(socket_path, fs::Permissions::from_mode(0o600));
    daemon_debug_log(debug, &format!("daemon listening on {} (history={})", socket_path.display(), history_path.display()));

    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(s) => s,
            Err(_) => continue,
        };

        let raw = match read_daemon_message(&mut stream) {
            Some(r) => r,
            None => {
                let _ = write_daemon_message(&mut stream, &error_frame("invalid request"));
                continue;
            }
        };

        let kind = match raw.get(FRAME_HEADER_BYTES) {
            Some(&k) => k,
            None => {
                let _ = write_daemon_message(&mut stream, &error_frame("invalid request"));
                continue;
            }
        };

        if kind == FRAME_PING_REQUEST {
            let valid = FrameReader::new(&raw, FRAME_PING_REQUEST).is_some_and(|r| r.done());
            if valid {
                let pong = FrameWriter::new(FRAME_PONG_RESPONSE).finish().unwrap();
                let _ = write_daemon_message(&mut stream, &pong);
            } else {
                let _ = write_daemon_message(&mut stream, &error_frame("invalid request"));
            }
            continue;
        }

        if kind != FRAME_SEARCH_REQUEST {
            let _ = write_daemon_message(&mut stream, &error_frame("unknown frame type"));
            continue;
        }

        let mut reader = match FrameReader::new(&raw, FRAME_SEARCH_REQUEST) {
            Some(r) => r,
            None => {
                let _ = write_daemon_message(&mut stream, &error_frame("invalid request"));
                continue;
            }
        };

        let query = match reader.string() {
            Some(q) => q,
            None => {
                let _ = write_daemon_message(&mut stream, &error_frame("invalid request"));
                continue;
            }
        };

        let candidate_indices = if reader.bool().unwrap_or(false) {
            let count = reader.u32().unwrap_or(0);
            let mut indices = Vec::with_capacity(count);
            for _ in 0..count {
                if let Some(idx) = reader.u64() {
                    indices.push(idx);
                }
            }
            Some(indices)
        } else {
            None
        };

        let limit = if reader.bool().unwrap_or(false) {
            reader.i64().map(|l| l as usize)
        } else {
            None
        };

        let cwd = reader.optional_string().unwrap_or(None);
        if !reader.done() {
            let _ = write_daemon_message(&mut stream, &error_frame("invalid request"));
            continue;
        }

        let new_sig = history_file_signature(history_path);
        if new_sig != signature {
            history_state.refresh();
            signature = new_sig;
        }

        let bound_candidates = candidate_indices.map(|indices| {
            let max_idx = history_state.len().saturating_sub(1);
            indices.into_iter().filter(|&i| i <= max_idx).collect::<Vec<usize>>()
        });

        let response = match history_state.search_response(
            &query,
            bound_candidates.as_deref(),
            limit,
            cwd.as_deref(),
        ) {
            Ok(resp) => resp,
            Err(e) => error_frame(e),
        };

        let _ = write_daemon_message(&mut stream, &response);
    }

    let _ = fs::remove_file(socket_path);
    0
}
