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
