pub mod completion;
pub mod daemon;
pub mod db;
pub mod input;
pub mod layout;
pub mod protocol;
pub mod render;
pub mod search;
pub mod syntax_highlighting;
pub mod terminal;
pub mod ui;

#[cfg(test)]
mod tests {
    use super::*;
    use completion::*;
    use db::*;
    use layout::*;
    use protocol::*;
    use search::*;
    use syntax_highlighting::*;

    #[test]
    fn binary_search_request_is_versioned_and_length_prefixed() {
        let frame = serialize_search_request_bytes(
            "git st",
            Some(&[1, 3, 8]),
            Some(100),
            Some("/repo"),
        )
        .unwrap();
        assert_eq!(&frame[..4], &FRAME_MAGIC);
        assert_eq!(
            u32::from_le_bytes(frame[4..8].try_into().unwrap()) as usize,
            frame.len() - FRAME_HEADER_BYTES
        );

        let mut reader = FrameReader::new(&frame, FRAME_SEARCH_REQUEST).unwrap();
        assert_eq!(reader.string().as_deref(), Some("git st"));
        assert_eq!(reader.bool(), Some(true));
        assert_eq!(reader.u32(), Some(3));
        assert_eq!(reader.u64(), Some(1));
        assert_eq!(reader.u64(), Some(3));
        assert_eq!(reader.u64(), Some(8));
        assert_eq!(reader.bool(), Some(true));
        assert_eq!(reader.i64(), Some(100));
        assert_eq!(reader.optional_string().unwrap().as_deref(), Some("/repo"));
        assert!(reader.done());
    }

    #[test]
    fn binary_search_response_round_trips() {
        let mut writer = FrameWriter::new(FRAME_SEARCH_RESPONSE);
        writer.byte(1);
        writer.u32(2).unwrap();
        writer.u64(3).unwrap();
        writer.u64(8).unwrap();
        writer.u32(1).unwrap();
        writer.string("git status --short").unwrap();
        writer.i64(72);
        writer.byte(0);
        writer.i64(-3);
        writer.optional_string(Some("/repo")).unwrap();
        writer.byte(0);
        writer.u32(3).unwrap();
        writer.string("git").unwrap();
        writer.string("status").unwrap();
        writer.string("--short").unwrap();

        assert_eq!(
            parse_search_response_bytes(&writer.finish().unwrap()),
            Some((
                vec![ParsedMatchItem {
                    text: "git status --short".to_string(),
                    score: 72,
                    exact: false,
                    recency: -3,
                    cwd: Some("/repo".to_string()),
                    failed: false,
                    words: vec!["git".to_string(), "status".to_string(), "--short".to_string()],
                }],
                Some(vec![3, 8]),
            ))
        );
    }

    #[test]
    fn compact_word_field_does_not_enlarge_native_candidate_layout() {
        assert_eq!(
            std::mem::size_of::<CompactWords>(),
            std::mem::size_of::<Box<[Box<str>]>>()
        );
    }

    #[test]
    fn compact_words_borrow_the_candidate_text() {
        let source = "git status --short";
        let words = CompactWords::new(
            source,
            vec!["git".to_string(), "status".to_string(), "--short".to_string()],
        );
        assert!(words.is_compact());
        assert!(!words.is_packed());
        assert_eq!(words.get(1), Some((4, 10)));
    }

    #[test]
    fn shell_transformed_words_share_one_packed_fallback() {
        let words = CompactWords::new(
            r"printf hello\ world",
            vec!["printf".to_string(), "hello world".to_string()],
        );
        assert!(words.is_compact());
        assert_eq!(words.packed_source(), Some("printfhello world"));
        assert_eq!(words.get(1), Some((6, 17)));
    }

    #[test]
    fn oversized_candidates_use_wide_offsets_without_truncation() {
        let source = format!("{}word", "x".repeat(u16::MAX as usize + 1));
        let words = CompactWords::new(&source, vec!["word".to_string()]);
        assert!(words.is_wide());
        assert!(!words.is_packed());
        assert_eq!(
            words.get(0),
            Some((u16::MAX as usize + 1, u16::MAX as usize + 5))
        );
    }

    #[test]
    fn visual_row_wrapping_and_unicode_width() {
        let rows = build_query_visual_rows("echo hello world", 10, None);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].text, "echo hello");
        assert_eq!(rows[1].text, " world");

        let (r, c) = query_cursor_visual_position(&rows, 5);
        assert_eq!(r, 0);
        assert_eq!(c, 5);
    }

    #[test]
    fn shell_token_boundaries() {
        let (s, e) = token_bounds("git commit -m 'initial commit'", 18);
        assert_eq!(&"git commit -m 'initial commit'"[s..e], "'initial commit'");
    }

    #[test]
    fn flex_matching_subsequence() {
        let q: Vec<char> = "gst".chars().collect();
        assert!(match_flex(&q, "git status", "git status").is_some());
        assert!(match_flex(&q, "ls -la", "ls -la").is_none());
    }

    #[test]
    fn syntax_highlighter_tokens() {
        let mut hl = IncrementalHighlighter::new();
        let tokens = hl.highlight("if echo 'hello' | grep foo; then fi");
        assert_eq!(tokens[0], KEYWORD); // if
        assert_eq!(tokens[8], STRING);  // '
        assert_eq!(tokens[16], OPERATOR); // |
    }

    #[test]
    fn path_completion_env_vars() {
        std::env::set_var("HOME", "/Users/alex");
        let res = expand_path_completion_environment("$HOME/Desktop");
        assert_eq!(
            res,
            Some((
                "/Users/alex/Desktop".to_string(),
                "$HOME".to_string(),
                "/Desktop".to_string()
            ))
        );
    }

    #[test]
    fn runtime_path_completions_rank_prefix_order_and_length() {
        let entries = vec![
            DirectoryListingEntry { name: "old_music".to_string(), is_dir: false },
            DirectoryListingEntry { name: "music_manifest".to_string(), is_dir: false },
            DirectoryListingEntry { name: "music".to_string(), is_dir: true },
            DirectoryListingEntry { name: "music folder".to_string(), is_dir: true },
        ];

        let ranked = top_ranked_directory_entries("music", &entries);
        assert_eq!(
            ranked.into_iter().map(|entry| entry.name).collect::<Vec<_>>(),
            vec!["music", "music folder", "music_manifest", "old_music"]
        );

        let multiword_entries = vec![
            DirectoryListingEntry { name: "archive music folder".to_string(), is_dir: true },
            DirectoryListingEntry { name: "museum_file_old".to_string(), is_dir: false },
            DirectoryListingEntry { name: "music folder".to_string(), is_dir: true },
        ];
        let ranked = top_ranked_directory_entries("mu fol", &multiword_entries);
        assert_eq!(
            ranked.into_iter().map(|entry| entry.name).collect::<Vec<_>>(),
            vec!["music folder", "archive music folder", "museum_file_old"]
        );
    }

    #[test]
    fn featured_runtime_completion_moves_matching_history_result_to_the_top() {
        let result = |text: &str, runtime_completion: bool| render::MatchResult {
            text: text.to_string(),
            score: 0,
            exact: false,
            recency: 0,
            cwd: None,
            text_lower: None,
            runtime_completion,
            failed: false,
            words: Vec::new(),
        };
        let history = vec![result("other history", false), result("/repo/music", false)];
        let runtime = vec![result("/repo/music", true), result("/repo/music_manifest", true)];

        let merged = insert_runtime_completions(history, runtime, 1);
        assert_eq!(
            merged.iter().map(|item| item.text.as_str()).collect::<Vec<_>>(),
            vec!["/repo/music", "other history", "/repo/music_manifest"]
        );
        assert!(merged[0].runtime_completion);
    }

    #[test]
    fn sqlite_custom_history_lifecycle() {
        let temp_dir = std::env::temp_dir().join(format!("zfh_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&temp_dir);
        let db_path = temp_dir.join("history.db");

        assert!(ensure_custom_history_file(&db_path).is_ok());

        let cwd = "/test/repo";
        let timestamp = "timestamp-is-not-used-for-status-updates";
        assert!(append_custom_history_entry(&db_path, "cargo check", cwd, &timestamp));

        let (history, watermark, rev) = build_native_custom_history_candidates(&db_path, None);
        assert_eq!(history.len(), 1);
        assert_eq!(watermark.unwrap().command, "cargo check");
        assert_eq!(rev, 0);

        assert!(update_custom_history_exit_status(&db_path, "cargo check", cwd, 1));
        let (history2, _, rev2) = build_native_custom_history_candidates(&db_path, None);
        assert_eq!(history2.candidates[0].failed, true);
        assert_eq!(rev2, 1);

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn custom_history_migration_matches_python_schema_behavior() {
        use rusqlite::Connection;

        let path = std::env::temp_dir().join(format!(
            "zfh_schema_migration_{}_{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("unnamed")
        ));
        let _ = std::fs::remove_file(&path);

        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "
            CREATE TABLE custom_history (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                command TEXT NOT NULL,
                cwd TEXT NOT NULL,
                timestamp TEXT NOT NULL,
                status_revision INTEGER NOT NULL DEFAULT 0
            );
            INSERT INTO custom_history(command, cwd, timestamp, status_revision)
            VALUES('cargo test', '/repo', 'timestamp', 7);
            ",
        )
        .unwrap();
        drop(conn);

        ensure_custom_history_file(&path).unwrap();

        let conn = Connection::open(&path).unwrap();
        let columns: Vec<String> = conn
            .prepare("PRAGMA table_info(custom_history)")
            .unwrap()
            .query_map([], |row| row.get(1))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(columns.iter().any(|column| column == "failed"));
        assert!(columns.iter().any(|column| column == "status_revision"));

        let metadata_revision: i64 = conn
            .query_row(
                "SELECT status_revision FROM custom_history_metadata WHERE id = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(metadata_revision, 7);

        for index_name in [
            "idx_custom_history_command_cwd",
            "idx_custom_history_id_desc",
            "idx_custom_history_status_revision",
        ] {
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = ?",
                    [index_name],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "missing migrated index {index_name}");
        }

        drop(conn);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn history_file_signature_contains_full_modification_time() {
        use std::os::unix::fs::MetadataExt;

        let path = std::env::temp_dir().join(format!(
            "zfh_signature_test_{}_{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("unnamed")
        ));
        std::fs::write(&path, b"history").unwrap();

        let metadata = std::fs::metadata(&path).unwrap();
        assert_eq!(
            daemon::history_file_signature(&path),
            (metadata.mtime(), metadata.mtime_nsec(), metadata.size())
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn daemon_custom_history_refresh_reads_one_sqlite_snapshot() {
        use rusqlite::Connection;

        let path = std::env::temp_dir().join(format!(
            "zfh_refresh_snapshot_{}_{}.db",
            std::process::id(),
            std::thread::current().name().unwrap_or("unnamed")
        ));
        let _ = std::fs::remove_file(&path);
        ensure_custom_history_file(&path).unwrap();
        assert!(append_custom_history_entry(
            &path,
            "first command",
            "/repo",
            "first timestamp"
        ));

        let mut reader = Connection::open(&path).unwrap();
        reader
            .pragma_update(None, "journal_mode", "WAL")
            .unwrap();

        let first_snapshot = daemon::read_custom_history_refresh_snapshot_with_hook(
            &mut reader,
            1,
            0,
            || {
                let writer = Connection::open(&path).unwrap();
                writer
                    .execute_batch(
                        "
                        BEGIN IMMEDIATE;
                        INSERT INTO custom_history(
                            command, cwd, timestamp, failed, status_revision
                        ) VALUES('concurrent command', '/repo', 'second timestamp', 1, 1);
                        UPDATE custom_history_metadata SET status_revision = 1 WHERE id = 1;
                        COMMIT;
                        ",
                    )
                    .unwrap();
            },
        )
        .unwrap();

        assert_eq!(first_snapshot.rows.len(), 1);
        assert_eq!(first_snapshot.rows[0].1, "first command");
        assert!(first_snapshot.status_rows.is_empty());
        assert_eq!(first_snapshot.revision, Some(0));

        let next_snapshot = daemon::read_custom_history_refresh_snapshot_with_hook(
            &mut reader,
            1,
            0,
            || {},
        )
        .unwrap();
        assert_eq!(next_snapshot.rows.len(), 2);
        assert_eq!(next_snapshot.status_rows, vec![(1, 0)]);
        assert_eq!(next_snapshot.revision, Some(1));

        drop(reader);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn spawned_daemon_process_starts_a_new_session() {
        use std::io::Read;
        use std::os::fd::FromRawFd;
        use std::os::unix::process::CommandExt;
        use std::process::Command;

        let parent_session = unsafe { libc::getsid(0) };
        assert_ne!(parent_session, -1);

        let mut pipe_fds = [-1; 2];
        assert_eq!(unsafe { libc::pipe(pipe_fds.as_mut_ptr()) }, 0);
        let read_fd = pipe_fds[0];
        let write_fd = pipe_fds[1];

        let mut command = Command::new("/usr/bin/true");
        daemon::detach_daemon_process(&mut command);
        unsafe {
            command.pre_exec(move || {
                let child_session = libc::getsid(0);
                let bytes = child_session.to_ne_bytes();
                let written = libc::write(
                    write_fd,
                    bytes.as_ptr().cast::<libc::c_void>(),
                    bytes.len(),
                );
                libc::close(write_fd);
                if written == bytes.len() as isize {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                }
            });
        }

        let mut child = command.spawn().unwrap();
        unsafe { libc::close(write_fd) };

        let mut session_bytes = [0_u8; std::mem::size_of::<libc::pid_t>()];
        let mut pipe_reader = unsafe { std::fs::File::from_raw_fd(read_fd) };
        pipe_reader.read_exact(&mut session_bytes).unwrap();
        assert!(child.wait().unwrap().success());

        let child_session = libc::pid_t::from_ne_bytes(session_bytes);
        assert_ne!(child_session, parent_session);
    }

    #[test]
    fn plain_history_replaces_invalid_utf8_without_discarding_entries() {
        let path = std::env::temp_dir().join(format!(
            "zfh_history_invalid_utf8_{}_{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("unnamed")
        ));
        std::fs::write(&path, b"git \xffstatus\ncargo test\n").unwrap();

        let entries = load_plain_zsh_history(&path);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, "cargo test");
        assert_eq!(entries[1].0, "git \u{fffd}status");

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn single_ascii_query_rejects_unicode_mask_collisions() {
        let inputs = vec![
            make_candidate_input("café before".to_string(), None, false).unwrap(),
            make_candidate_input("install package".to_string(), None, false).unwrap(),
            make_candidate_input("café after".to_string(), None, false).unwrap(),
        ];
        let history = NativeHistory::new(inputs);
        let query_words = vec!["i".to_string()];

        let (ranked, matched_indices) = history.search_ranked(
            "i",
            "i",
            &query_words,
            &query_words,
            None,
            None,
            Some(1),
            usize::MAX,
        );

        assert_eq!(ranked, vec![(1, 0)]);
        assert_eq!(matched_indices, Some(vec![1]));
    }
}
