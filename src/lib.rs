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
    use chrono::Utc;
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
        let timestamp = Utc::now().to_rfc3339();
        assert!(append_custom_history_entry(&db_path, "cargo check", cwd, &timestamp));

        let (history, watermark, rev) = build_native_custom_history_candidates(&db_path, None);
        assert_eq!(history.len(), 1);
        assert_eq!(watermark.unwrap().command, "cargo check");
        assert_eq!(rev, 0);

        assert!(update_custom_history_exit_status(&db_path, "cargo check", cwd, 1, 3600));
        let (history2, _, rev2) = build_native_custom_history_candidates(&db_path, None);
        assert_eq!(history2.candidates[0].failed, true);
        assert_eq!(rev2, 1);

        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
