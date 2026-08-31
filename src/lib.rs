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
    fn results_always_use_every_other_terminal_row() {
        assert_eq!(result_row_offset(0), 1);
        assert_eq!(result_row_offset(1), 3);
        assert_eq!(result_row_offset(2), 5);
        assert_eq!(results_fitting_rows(7), 3);
        assert_eq!(results_fitting_rows(5), 2);
        assert_eq!(results_fitting_rows(4), 2);
    }

    #[test]
    fn shell_token_boundaries() {
        let (s, e) = token_bounds("git commit -m 'initial commit'", 18);
        assert_eq!(&"git commit -m 'initial commit'"[s..e], "'initial commit'");
    }

    #[test]
    fn quoted_http_urls_select_only_their_contents() {
        assert_eq!(
            quoted_http_selection(r#"open "https://example.com/a b""#),
            Some((6, 29))
        );
        assert_eq!(quoted_http_selection("open 'http://example.com'"), Some((6, 24)));
    }

    #[test]
    fn quoted_non_urls_and_unquoted_urls_are_not_selected() {
        assert_eq!(quoted_http_selection(r#"echo "hello""#), None);
        assert_eq!(quoted_http_selection("open https://example.com"), None);
        assert_eq!(quoted_http_selection(r#"open "ftp://example.com""#), None);
    }

    #[test]
    fn flex_matching_subsequence() {
        let q: Vec<char> = "gst".chars().collect();
        assert!(match_flex(&q, "git status", "git status").is_some());
        assert!(match_flex(&q, "ls -la", "ls -la").is_none());
    }

    #[test]
    fn flex_metrics_and_majority_reward_continuity_and_shorter_candidates() {
        let query: Vec<char> = "abc".chars().collect();
        let contiguous = match_flex_metrics(&query, "abc-long").unwrap();
        let gapped = match_flex_metrics(&query, "a-b-c").unwrap();
        let short = match_flex_metrics(&query, "abc").unwrap();
        let scores = pairwise_majority_scores(&[contiguous, gapped, short], None);

        assert_eq!(contiguous.longest_run, 3);
        assert_eq!(gapped.longest_run, 1);
        assert!(scores[2] > scores[0]);
        assert!(scores[0] > scores[1]);
    }

    #[test]
    fn flex_metrics_measure_runs_across_a_compacted_multiword_query() {
        let query: Vec<char> = "gitcommit".chars().collect();
        let separated = match_flex_metrics(&query, "git commit").unwrap();
        let contiguous = match_flex_metrics(&query, "gitcommit").unwrap();

        assert_eq!(
            separated,
            FlexMetrics {
                longest_run: 6,
                adjacent_pairs: 7,
                span: 10,
                candidate_len: 10,
            }
        );
        assert_eq!(
            contiguous,
            FlexMetrics {
                longest_run: 9,
                adjacent_pairs: 8,
                span: 9,
                candidate_len: 9,
            }
        );
    }

    #[test]
    fn pairwise_majority_is_not_lexicographic() {
        let longer_run = FlexMetrics {
            longest_run: 4,
            adjacent_pairs: 3,
            span: 10,
            candidate_len: 11,
        };
        let better_overall = FlexMetrics {
            longest_run: 3,
            adjacent_pairs: 4,
            span: 7,
            candidate_len: 8,
        };

        assert_eq!(
            pairwise_majority_scores(&[longer_run, better_overall], None),
            vec![-1, 1]
        );
    }

    #[test]
    fn normalized_admission_can_favor_balance_over_the_longest_run() {
        let longer_run = FlexMetrics {
            longest_run: 4,
            adjacent_pairs: 3,
            span: 10,
            candidate_len: 11,
        };
        let balanced = FlexMetrics {
            longest_run: 3,
            adjacent_pairs: 4,
            span: 7,
            candidate_len: 8,
        };

        assert!(
            normalized_flex_admission_score(balanced, 6, None)
                > normalized_flex_admission_score(longer_run, 6, None)
        );
    }

    #[test]
    fn normalized_admission_gives_recency_one_bounded_component() {
        let metrics = FlexMetrics {
            longest_run: 3,
            adjacent_pairs: 2,
            span: 3,
            candidate_len: 3,
        };
        let without_recency = normalized_flex_admission_score(metrics, 3, None);
        let newest = normalized_flex_admission_score(metrics, 3, Some(1_000_000));
        let oldest = normalized_flex_admission_score(metrics, 3, Some(0));

        assert_eq!(newest - without_recency, 1_000_000);
        assert_eq!(oldest, without_recency);
    }

    #[test]
    fn history_recency_casts_one_pairwise_vote() {
        let long_but_wide = FlexMetrics {
            longest_run: 4,
            adjacent_pairs: 3,
            span: 20,
            candidate_len: 20,
        };
        let short_but_split = FlexMetrics {
            longest_run: 3,
            adjacent_pairs: 2,
            span: 6,
            candidate_len: 6,
        };

        // With four quality metrics, each candidate wins two and the pair ties.
        assert_eq!(
            pairwise_majority_scores(&[long_but_wide, short_but_split], None),
            vec![0, 0]
        );
        // Lower history indices are newer. Recency breaks the 2-2 tie, but is
        // still only one vote rather than an overriding final tie-breaker.
        assert_eq!(
            pairwise_majority_scores(&[long_but_wide, short_but_split], Some(&[9, 2]),),
            vec![-1, 1]
        );
    }

    #[test]
    fn identical_flex_quality_prefers_the_more_recent_history_entry() {
        let metrics = FlexMetrics {
            longest_run: 2,
            adjacent_pairs: 1,
            span: 4,
            candidate_len: 8,
        };

        assert_eq!(
            pairwise_majority_scores(&[metrics, metrics], Some(&[0, 5])),
            vec![1, -1]
        );
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

        let flex_entries = vec![
            DirectoryListingEntry { name: "xxa-bx-c".to_string(), is_dir: false },
            DirectoryListingEntry { name: "zzabxc-long".to_string(), is_dir: false },
        ];
        let ranked = top_ranked_directory_entries("abc", &flex_entries);
        assert_eq!(
            ranked.into_iter().map(|entry| entry.name).collect::<Vec<_>>(),
            vec!["zzabxc-long", "xxa-bx-c"]
        );
    }

    #[test]
    fn runtime_flex_ranking_uses_pairwise_majority() {
        let entries = vec![
            DirectoryListingEntry {
                name: "xabcd-xe-xf".to_string(),
                is_dir: false,
            },
            DirectoryListingEntry {
                name: "xabc-def".to_string(),
                is_dir: false,
            },
        ];

        let ranked = top_ranked_directory_entries("abcdef", &entries);
        assert_eq!(
            ranked.into_iter().map(|entry| entry.name).collect::<Vec<_>>(),
            vec!["xabc-def", "xabcd-xe-xf"]
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
            history_match: !runtime_completion,
            runtime_completion_span: None,
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
        assert!(merged[0].history_match);
    }

    #[test]
    fn one_history_result_stays_between_first_and_remaining_runtime_completions() {
        let result = |text: &str, runtime_completion: bool| render::MatchResult {
            text: text.to_string(),
            score: 0,
            exact: false,
            recency: 0,
            cwd: None,
            text_lower: None,
            runtime_completion,
            history_match: !runtime_completion,
            runtime_completion_span: None,
            failed: false,
            words: Vec::new(),
        };
        let history = vec![result("history", false)];
        let runtime = vec![
            result("runtime 1", true),
            result("runtime 2", true),
            result("runtime 3", true),
        ];

        let merged = insert_runtime_completions(history, runtime, 1);
        assert_eq!(
            merged.iter().map(|item| item.text.as_str()).collect::<Vec<_>>(),
            vec!["runtime 1", "history", "runtime 2", "runtime 3"]
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

        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].0, 1);
        assert_eq!(matched_indices, Some(vec![1]));
    }

    #[test]
    fn contiguous_single_word_match_precedes_same_cwd_flex_match() {
        let inputs = vec![
            make_candidate_input("music_xmanifest".to_string(), Some("/repo".to_string()), false)
                .unwrap(),
            make_candidate_input(
                "cd ~/Music && find . -type f | sort > ~/Desktop/music_manifest.txt".to_string(),
                Some("/other".to_string()),
                false,
            )
            .unwrap(),
        ];
        let history = NativeHistory::new(inputs);
        let query_words = vec!["music_manifest".to_string()];
        let current_cwd = history.cwd_interner.get("/repo").unwrap();

        let (ranked, _) = history.search_ranked(
            "music_manifest",
            "music_manifest",
            &query_words,
            &query_words,
            Some(current_cwd),
            None,
            Some(2),
            usize::MAX,
        );

        assert_eq!(ranked.iter().map(|item| item.0).collect::<Vec<_>>(), vec![1, 0]);
    }

    #[test]
    fn history_uses_pairwise_majority_only_within_a_flex_bucket() {
        let inputs = vec![
            make_candidate_input("axbxc".to_string(), None, false).unwrap(),
            make_candidate_input("abxc-long".to_string(), None, false).unwrap(),
            make_candidate_input("abxc-newest-tie".to_string(), None, false).unwrap(),
            make_candidate_input("abxc-oldest-tie".to_string(), None, false).unwrap(),
        ];
        let history = NativeHistory::new(inputs);
        let query_words = vec!["abc".to_string()];

        let (ranked, _) = history.search_ranked(
            "abc",
            "abc",
            &query_words,
            &query_words,
            None,
            None,
            Some(4),
            usize::MAX,
        );

        assert_eq!(ranked[0].0, 1);
        assert_eq!(ranked[1].0, 2);
        assert_eq!(ranked[2].0, 3);
        assert_eq!(ranked[3].0, 0);
    }

    #[test]
    fn words_in_order_bucket_preserves_recency_instead_of_pairwise_ranking() {
        let inputs = vec![
            make_candidate_input("ab-very-very-long".to_string(), None, false).unwrap(),
            make_candidate_input("abx".to_string(), None, false).unwrap(),
        ];
        let history = NativeHistory::new(inputs);
        let query_words = vec!["ab".to_string()];

        let (ranked, _) = history.search_ranked(
            "ab",
            "ab",
            &query_words,
            &query_words,
            None,
            None,
            Some(2),
            usize::MAX,
        );

        assert_eq!(ranked.iter().map(|item| item.0).collect::<Vec<_>>(), vec![0, 1]);
    }

    #[test]
    fn flex_heap_keeps_a_better_match_found_after_the_overscan_cap() {
        let inputs = vec![
            make_candidate_input("axbxc-newest".to_string(), None, false).unwrap(),
            make_candidate_input("aybyc-recent".to_string(), None, false).unwrap(),
            make_candidate_input("abxc-older".to_string(), None, false).unwrap(),
        ];
        let history = NativeHistory::new(inputs);
        let query_words = vec!["abc".to_string()];

        let (ranked, _) = history.search_ranked(
            "abc",
            "abc",
            &query_words,
            &query_words,
            None,
            None,
            Some(1),
            usize::MAX,
        );

        assert_eq!(ranked.iter().map(|item| item.0).collect::<Vec<_>>(), vec![2]);
    }

    #[test]
    fn normalized_pool_admits_a_balanced_match_despite_a_shorter_run() {
        let inputs = vec![
            make_candidate_input("xabcxdef".to_string(), None, false).unwrap(),
            make_candidate_input("xabcdxxexxf".to_string(), None, false).unwrap(),
            make_candidate_input("yabcdyyeyyf".to_string(), None, false).unwrap(),
        ];
        let history = NativeHistory::new(inputs);
        let query_words = vec!["abcdef".to_string()];

        let (ranked, _) = history.search_ranked(
            "abcdef",
            "abcdef",
            &query_words,
            &query_words,
            None,
            None,
            Some(1),
            usize::MAX,
        );

        // The 2x pool has room for only two candidates. The old lexicographic
        // rule retained both four-character runs; normalized admission keeps
        // this tighter, shorter three-character-run match for the tournament.
        assert_eq!(ranked.iter().map(|item| item.0).collect::<Vec<_>>(), vec![0]);
    }

    #[test]
    fn match_separators_are_ignored_without_crossing_other_characters() {
        assert_eq!(compact_query("one-two_three"), "onetwothree".chars().collect::<Vec<_>>());
        assert!(word_starts_with_ignoring_separators("one-two", "onetwo"));
        assert!(word_starts_with_ignoring_separators("one_two", "one-two"));
        assert!(!word_starts_with_ignoring_separators("one-x-two", "onetwo"));
        assert!(words_appear_in_order(
            &["one".to_string(), "two".to_string()],
            "echo one-two"
        ));
        assert!(words_appear_in_order(
            &["onetwo".to_string()],
            "echo one_two"
        ));
        assert!(!words_appear_in_order(
            &["onetwo".to_string()],
            "echo one x two"
        ));
    }

    #[test]
    fn separator_only_query_words_do_not_change_history_buckets() {
        let words_for = |query: &str| {
            (
                normalize_matching_words(shell_words_for_matching(query)),
                normalize_matching_words(
                    query
                        .to_lowercase()
                        .split_whitespace()
                        .map(str::to_string)
                        .collect(),
                ),
            )
        };
        let inputs = vec![
            make_candidate_input(
                "git commit -am \"made -_ ignored in searching\"".to_string(),
                None,
                false,
            )
            .unwrap(),
            make_candidate_input(
                "git commit -am \"fixed contiguous word priority\"".to_string(),
                None,
                false,
            )
            .unwrap(),
            make_candidate_input("git commit -am \"updated demo image\"".to_string(), None, false)
                .unwrap(),
        ];
        let history = NativeHistory::new(inputs);
        let run = |query: &str| {
            let (prefix_words, ordered_words) = words_for(query);
            history
                .search_ranked(
                    query,
                    query.trim(),
                    &prefix_words,
                    &ordered_words,
                    None,
                    None,
                    Some(3),
                    usize::MAX,
                )
                .0
        };

        assert_eq!(run("git commit"), run("git commit -"));
        assert_eq!(run("git commit am"), run("git commit -am"));
        assert_eq!(run(""), run("-"));
    }
}
