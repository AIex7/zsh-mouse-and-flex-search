use std::collections::HashMap;
use std::os::unix::io::RawFd;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::completion::*;
use crate::daemon::HistoryDaemonClient;
use crate::db::normalize_cwd_value;
use crate::input::*;
use crate::layout::*;
use crate::render::*;
use crate::syntax_highlighting::IncrementalHighlighter;
use crate::terminal::*;

extern "C" {
    fn ctermid(s: *mut libc::c_char) -> *mut libc::c_char;
}

pub fn filter_exact_query_match(query: &str, results: Vec<MatchResult>) -> Vec<MatchResult> {
    let normalized = query.trim().to_lowercase();
    if normalized.is_empty() {
        return results;
    }
    results
        .into_iter()
        .filter(|item| item.text.trim().to_lowercase() != normalized)
        .collect()
}

pub struct SearchRequest {
    pub query: String,
    pub candidate_indices: Option<Vec<usize>>,
    pub cwd: String,
}

pub struct SearchResponse {
    pub query: String,
    pub candidate_indices: Option<Vec<usize>>,
    pub results: Vec<MatchResult>,
    pub error: bool,
}

pub fn run(
    inline_with_prompt: bool,
    history_client: Arc<HistoryDaemonClient>,
    empty_space_command: Option<String>,
) -> Option<(String, bool)> {
    let mut tty_fd_opt: Option<RawFd> = None;
    let mut opened_tty_fd: Option<RawFd> = None;

    let cterm_path = unsafe {
        let mut buf = [0 as libc::c_char; 1024];
        let ptr = ctermid(buf.as_mut_ptr());
        if !ptr.is_null() {
            std::ffi::CStr::from_ptr(ptr).to_str().unwrap_or("/dev/tty")
        } else {
            "/dev/tty"
        }
    };

    for path in &["/dev/tty", cterm_path] {
        if let Ok(c_path) = std::ffi::CString::new(*path) {
            let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_RDWR | libc::O_NOCTTY) };
            if fd >= 0 {
                if unsafe { libc::isatty(fd) } != 0 {
                    tty_fd_opt = Some(fd);
                    opened_tty_fd = Some(fd);
                    break;
                } else {
                    unsafe { libc::close(fd) };
                }
            }
        }
    }

    if tty_fd_opt.is_none() {
        let stdin_fd = 0;
        if unsafe { libc::isatty(stdin_fd) } != 0 {
            tty_fd_opt = Some(stdin_fd);
        }
    }

    let fd = match tty_fd_opt {
        Some(f) => f,
        None => {
            eprintln!("zsh_flex_history: no usable TTY available for interactive mode");
            return None;
        }
    };

    let min_result_rows: usize = 4;
    let min_panel_rows: usize = 1 + min_result_rows;

    let _rt = match RawTerminal::enter(fd, opened_tty_fd.is_some()) {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("zsh_flex_history: failed to enter raw terminal: {}", e);
            if let Some(opened) = opened_tty_fd {
                unsafe { libc::close(opened) };
            }
            return None;
        }
    };

    let (
        cursor_color_override,
        background_color_override,
        selection_background_color_override,
        selection_foreground_color_override,
    ) = query_terminal_colors(fd);
    let cursor_text_color = background_color_override.as_deref().unwrap_or(DORIC_FG_MAIN);
    let selection_style = style(StyleOptions {
        fg_rgb: Some(
            selection_foreground_color_override
                .as_deref()
                .unwrap_or(DORIC_FG_BLUE),
        ),
        bg_rgb: Some(
            selection_background_color_override
                .as_deref()
                .unwrap_or(DORIC_BG_BLUE),
        ),
        ..Default::default()
    });
    let visual_cursor_bg_style = if let Some(color_hex) = &cursor_color_override {
        style(StyleOptions {
            fg_rgb: Some(cursor_text_color),
            bg_rgb: Some(color_hex),
            ..Default::default()
        })
    } else if let Ok(env_color) = std::env::var("ZSH_FLEX_HISTORY_CURSOR_COLOR") {
        let val = env_color.trim().to_lowercase();
        if val.starts_with('#') && val.len() == 7 {
            style(StyleOptions {
                fg_rgb: Some(cursor_text_color),
                bg_rgb: Some(&val),
                ..Default::default()
            })
        } else if let Some(slot) = ansi_color_from_env("ZSH_FLEX_HISTORY_CURSOR_COLOR", None) {
            style(StyleOptions {
                fg: if background_color_override.is_none() {
                    ansi_color_from_env("ZSH_FLEX_HISTORY_COLOR", None)
                } else {
                    None
                },
                fg_rgb: background_color_override.as_deref(),
                bg: Some(slot),
                ..Default::default()
            })
        } else {
            style(StyleOptions {
                fg_rgb: Some(cursor_text_color),
                bg_rgb: Some(DORIC_BG_ACCENT),
                ..Default::default()
            })
        }
    } else {
        style(StyleOptions {
            fg_rgb: Some(cursor_text_color),
            bg_rgb: Some(DORIC_BG_ACCENT),
            ..Default::default()
        })
    };

    let term_size = tty_terminal_size(fd, (120, 24));
    let mut term_lines = term_size.1;
    let pos = query_cursor_position(fd);
    let (mut start_row, mut start_col) = match pos {
        Some((r, c)) => (r, c),
        None => (term_lines.saturating_sub(1).max(1), 1),
    };

    start_row = start_row.clamp(1, term_lines);
    let mut space_below = term_lines.saturating_sub(start_row);

    let required_below = if inline_with_prompt {
        min_panel_rows.saturating_sub(1)
    } else {
        min_panel_rows
    };

    let scroll_rows = required_below.saturating_sub(space_below);
    if scroll_rows > 0 {
        term_write(fd, &format!("{}{}", move_to(term_lines, 1), "\n".repeat(scroll_rows)));
        start_row = start_row.saturating_sub(scroll_rows).max(1);
        space_below = term_lines.saturating_sub(start_row);
    }

    let mut anchor_row = if inline_with_prompt {
        start_row.max(1)
    } else if space_below >= 1 {
        start_row + 1
    } else {
        start_row.max(1)
    };
    let mut anchor_col = if inline_with_prompt {
        start_col.saturating_sub(1).max(1)
    } else {
        1
    };
    let mut panel_rows: usize;

    term_write(fd, &move_to(anchor_row, anchor_col));

    let current_cwd_text = normalize_cwd_value(&std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default());
    let current_cwd_path = PathBuf::from(&current_cwd_text);

    let mut query = String::new();
    let current_path_env = std::env::var("PATH").unwrap_or_default();
    let path_commands = history_client
        .get_path_commands(&current_path_env, &current_cwd_text)
        .unwrap_or_default();
    let mut syntax_highlighter = IncrementalHighlighter::with_commands(path_commands);
    let mut last_refresh_query: Option<String> = None;
    let mut last_refresh_results: Vec<String> = Vec::new();
    let mut last_refresh_query_rows = 1;
    let mut cursor_pos = 0;
    let mut skip_history_record = false;
    let mut sel_anchor: Option<usize> = None;
    let mut sel_end: Option<usize> = None;
    let mut selected = 0;
    let mut offset = 0;
    let mut results_expanded = false;
    let chosen: Option<String>;
    let mut query_start;
    let mut query_rows_used;
    let mut last_drawn_panel_rows;

    let loaded = history_client.search_history(
        "",
        None,
        Some(positive_int_from_env("ZSH_FLEX_HISTORY_MAX_RETURNED_RESULTS", 100) as i64),
        Some(&current_cwd_text),
    );
    let (initial_results, initial_matched_indices, mut history_load_error) = match loaded {
        Some((items, indices)) => {
            let res = items
                .into_iter()
                .map(|item| MatchResult {
                    text: item.text,
                    score: item.score,
                    exact: item.exact,
                    recency: item.recency,
                    cwd: item.cwd,
                    text_lower: None,
                    runtime_completion: false,
                    history_match: true,
                    runtime_completion_span: None,
                    failed: item.failed,
                    words: item.words,
                })
                .collect::<Vec<MatchResult>>();
            (res, indices, false)
        }
        None => (Vec::new(), None, true),
    };

    let mut match_cache: HashMap<String, (Option<Vec<usize>>, Vec<MatchResult>)> = HashMap::new();
    match_cache.insert("".to_string(), (initial_matched_indices.clone(), initial_results.clone()));
    let mut cache_order: Vec<String> = vec!["".to_string()];
    let cache_limit = 128;
    let mut displayed_results = initial_results;
    let mut displayed_matched_indices = initial_matched_indices;

    let (search_tx, search_rx): (Sender<Option<SearchRequest>>, Receiver<Option<SearchRequest>>) = channel();
    let (update_tx, update_rx): (Sender<SearchResponse>, Receiver<SearchResponse>) = channel();

    let client_clone = Arc::clone(&history_client);
    std::thread::spawn(move || {
        while let Ok(Some(req)) = search_rx.recv() {
            let remote = client_clone.search_history(
                &req.query,
                req.candidate_indices.as_deref(),
                Some(positive_int_from_env("ZSH_FLEX_HISTORY_MAX_RETURNED_RESULTS", 100) as i64),
                Some(&req.cwd),
            );
            let (results, matched_indices, error) = match remote {
                Some((items, indices)) => {
                    let res = items
                        .into_iter()
                        .map(|item| MatchResult {
                            text: item.text,
                            score: item.score,
                            exact: item.exact,
                            recency: item.recency,
                            cwd: item.cwd,
                            text_lower: None,
                            runtime_completion: false,
                            history_match: true,
                            runtime_completion_span: None,
                            failed: item.failed,
                            words: item.words,
                        })
                        .collect();
                    (res, indices, false)
                }
                None => (Vec::new(), None, true),
            };
            let _ = update_tx.send(SearchResponse {
                query: req.query,
                candidate_indices: matched_indices,
                results,
                error,
            });
        }
    });

    let mut queued_search_key: Option<String> = None;
    let mut mouse_enabled = false;
    let mut mouse_selecting = false;
    let mut kitty_keyboard_enabled = false;
    let kitty_keyboard_supported = supports_kitty_keyboard_protocol();
    let mut last_left_click_time = Instant::now();
    let mut last_left_click_row: i32 = -1;
    let mut last_left_click_col: i32 = -1;
    let mut left_click_count = 0;
    let mut render_state = PanelRenderState::default();
    let mut preferred_runtime_row: Option<usize> = None;
    let mut runtime_completion_cache: HashMap<(String, usize), Vec<MatchResult>> = HashMap::new();
    let mut prewarm_directory_started = false;

    let clear_panel_display = |fd: RawFd, a_row: usize, a_col: usize, inline: bool| {
        let (_, t_lines) = tty_terminal_size(fd, (120, 24));
        let clear_col = if inline { a_col } else { 1 };
        for row in a_row..=t_lines {
            term_write(fd, &format!("{}{}", move_to(row, if row == a_row { clear_col } else { 1 }), CLEAR_TO_END));
        }
    };

    let clear_panel_and_restore_cursor = |fd: RawFd,
                                          s_row: usize,
                                          s_col: usize,
                                          a_row: usize,
                                          a_col: usize,
                                          inline: bool,
                                          clear_display: bool,
                                          m_en: &mut bool,
                                          k_en: &mut bool| {
        if *k_en {
            term_write(fd, DISABLE_KITTY_KEYBOARD);
            *k_en = false;
        }
        if *m_en {
            term_write(fd, DISABLE_MOUSE);
            *m_en = false;
        }
        if clear_display {
            clear_panel_display(fd, a_row, a_col, inline);
        }
        term_write(fd, SHOW_CURSOR);
        term_write(fd, RESET);
        term_write(fd, &move_to(s_row, s_col));
    };

    let sync_mouse_mode = |query_chars: usize,
                           m_en: &mut bool,
                           k_en: &mut bool,
                           fd: RawFd| {
        let should_enable = query_chars > 0;
        if should_enable && !*m_en {
            term_write(fd, ENABLE_MOUSE);
            if kitty_keyboard_supported && !*k_en {
                term_write(fd, ENABLE_KITTY_KEYBOARD);
                *k_en = true;
            }
            *m_en = true;
        } else if !should_enable && *m_en {
            term_write(fd, DISABLE_MOUSE);
            if *k_en {
                term_write(fd, DISABLE_KITTY_KEYBOARD);
                *k_en = false;
            }
            *m_en = false;
        }
    };

    let move_cursor = |new_pos: usize,
                       select_mode: bool,
                       query_chars: usize,
                       c_pos: &mut usize,
                       s_anchor: &mut Option<usize>,
                       s_end: &mut Option<usize>| {
        let target = new_pos.min(query_chars);
        if select_mode {
            if s_anchor.is_none() {
                *s_anchor = Some(*c_pos);
            }
            *c_pos = target;
            *s_end = Some(*c_pos);
            if *s_anchor == *s_end {
                *s_anchor = None;
                *s_end = None;
            }
            return;
        }
        *c_pos = target;
        *s_anchor = None;
        *s_end = None;
    };

    let select_all_query = |query_chars: usize,
                            c_pos: &mut usize,
                            s_anchor: &mut Option<usize>,
                            s_end: &mut Option<usize>| {
        if query_chars == 0 {
            *s_anchor = None;
            *s_end = None;
            return;
        }
        *s_anchor = Some(0);
        *s_end = Some(query_chars);
        *c_pos = query_chars;
    };

    let clear_after_query_suffix = |fd: RawFd,
                                    query_str: &str,
                                    c_pos: usize,
                                    a_row: usize,
                                    a_col: usize,
                                    p_rows: usize| {
        let (cols, _) = tty_terminal_size(fd, (120, 24));
        let r_width = terminal_safe_render_width(cols, a_col);
        let q_width = query_text_render_width(r_width, 1);
        let cont_width = terminal_safe_render_width(cols, 1);
        let (q_start, _, _, _) = wrapped_query_layout(query_str, c_pos, q_width, p_rows, Some(cont_width), None);
        let q_rows = build_query_visual_rows(query_str, q_width, Some(cont_width));
        let (row_abs, col) = query_cursor_visual_position(&q_rows, query_str.chars().count());
        let row = row_abs.saturating_sub(q_start);
        let draw_col = if row_abs == 0 { a_col } else { 1 };
        term_write(fd, &format!("{}{}", move_to(a_row + row, draw_col + col + 2), CLEAR_TO_END));
    };

    let reanchor_from_position = |pos: (usize, usize),
                                  fd: RawFd,
                                  s_row: &mut usize,
                                  s_col: &mut usize,
                                  a_row: &mut usize,
                                  a_col: &mut usize,
                                  p_rows: &mut usize,
                                  query_str: &str,
                                  c_pos: usize,
                                  results_list: &[MatchResult],
                                  last_ref_query: &mut Option<String>,
                                  last_ref_results: &mut Vec<String>,
                                  last_ref_query_rows: &mut usize| {
        let (cols, t_lines) = tty_terminal_size(fd, (120, 24));
        let mut next_start_row = pos.0.clamp(1, t_lines);
        let next_start_col = pos.1;
        let mut sp_below = t_lines.saturating_sub(next_start_row);

        let req_below = if inline_with_prompt {
            min_panel_rows.saturating_sub(1)
        } else {
            min_panel_rows
        };
        let sc_rows = req_below.saturating_sub(sp_below);
        if sc_rows > 0 {
            term_write(fd, &format!("{}{}", move_to(t_lines, 1), "\n".repeat(sc_rows)));
            next_start_row = next_start_row.saturating_sub(sc_rows).max(1);
            sp_below = t_lines.saturating_sub(next_start_row);
        }

        let (next_anchor_row, next_anchor_col, next_panel_rows) = if inline_with_prompt {
            (next_start_row.max(1), next_start_col.saturating_sub(1).max(1), (t_lines - next_start_row.max(1) + 1).max(1))
        } else if sp_below >= 1 {
            (next_start_row + 1, 1, sp_below.max(1))
        } else {
            (next_start_row.max(1), 1, (t_lines - next_start_row.max(1) + 1).max(1))
        };

        *s_row = next_start_row;
        *s_col = next_start_col;
        *a_row = next_anchor_row;
        *a_col = next_anchor_col;
        *p_rows = next_panel_rows;

        let r_width = terminal_safe_render_width(cols, next_anchor_col);
        let q_width = query_text_render_width(r_width, 1);
        let cont_width = terminal_safe_render_width(cols, 1);
        let (q_start, _, q_rows_used, _) = wrapped_query_layout(query_str, c_pos, q_width, next_panel_rows, Some(cont_width), None);
        let current_results: Vec<String> = results_list.iter().take(2).map(|i| i.text.clone()).collect();

        if last_ref_query.as_deref() != Some(query_str) {
            let q_rows = build_query_visual_rows(query_str, q_width, Some(cont_width));
            let mut common_length = 0;
            if let Some(ref prev_q) = last_ref_query {
                let prev_chars: Vec<char> = prev_q.chars().collect();
                let curr_chars: Vec<char> = query_str.chars().collect();
                let limit = prev_chars.len().min(curr_chars.len());
                while common_length < limit && prev_chars[common_length] == curr_chars[common_length] {
                    common_length += 1;
                }
            }
            let (clear_row_abs, clear_col) = query_cursor_visual_position(&q_rows, common_length);
            let clear_row = clear_row_abs.saturating_sub(q_start);
            let clear_col_abs = (if clear_row_abs == 0 { next_anchor_col } else { 1 }) + clear_col + 1;
            for r in (next_anchor_row + clear_row)..(next_anchor_row + (*last_ref_query_rows).max(q_rows_used)) {
                term_write(fd, &format!("{}{}", move_to(r, if r == next_anchor_row + clear_row { clear_col_abs } else { 1 }), CLEAR_TO_END));
            }
            *last_ref_query = Some(query_str.to_string());
            *last_ref_query_rows = q_rows_used;
        }

        let q_rows = build_query_visual_rows(query_str, q_width, Some(cont_width));
        let (last_row_abs, last_col) = query_cursor_visual_position(&q_rows, query_str.chars().count());
        let last_row = last_row_abs.saturating_sub(q_start);
        let last_row_col = (if last_row_abs == 0 { next_anchor_col } else { 1 }) + last_col + 1;
        term_write(fd, &format!("{}{}", move_to(next_anchor_row + last_row, last_row_col), CLEAR_TO_END));

        for (res_idx, curr_res) in current_results.iter().enumerate() {
            let prev_res = last_ref_results.get(res_idx).map(|s| s.as_str()).unwrap_or("");
            if prev_res != curr_res {
                let res_row = next_anchor_row + q_rows_used + result_row_offset(res_idx);
                let clear_res_col = result_changed_suffix_col(prev_res, curr_res, next_anchor_col);
                term_write(fd, &format!("{}{}", move_to(res_row, clear_res_col), CLEAR_TO_END));
            }
        }
        *last_ref_results = current_results;

        let clear_after_results_row = if q_rows.len() > 1 {
            next_anchor_row + q_rows_used
        } else {
            next_anchor_row + q_rows_used + min_result_rows
        };
        for r in clear_after_results_row..=t_lines {
            term_write(fd, &format!("{}{}", move_to(r, 1), CLEAR_TO_END));
        }
        term_write(fd, &move_to(*a_row, *a_col));
    };

    let logical_cursor_terminal_position = |fd: RawFd, query_str: &str, c_pos: usize, a_row: usize, a_col: usize, p_rows: usize| -> (usize, usize) {
        let (cols, _) = tty_terminal_size(fd, (120, 24));
        let r_width = terminal_safe_render_width(cols, a_col);
        let q_width = query_text_render_width(r_width, 1);
        let cont_width = terminal_safe_render_width(cols, 1);
        let (q_start, _, _, _) = wrapped_query_layout(query_str, c_pos, q_width, p_rows, Some(cont_width), None);
        let q_rows = build_query_visual_rows(query_str, q_width, Some(cont_width));
        let (cursor_row_abs, cursor_col) = query_cursor_visual_position(&q_rows, c_pos);
        let cursor_row = cursor_row_abs.saturating_sub(q_start);
        let draw_col = if cursor_row_abs == 0 { a_col } else { 1 };
        (a_row + cursor_row, draw_col + cursor_col)
    };

    let mut skip_previous_cursor_clear = false;

    loop {
        term_write(fd, &move_to(start_row, start_col));

        while let Ok(update) = update_rx.try_recv() {
            if queued_search_key.as_deref() == Some(&update.query) {
                queued_search_key = None;
            }
            if !match_cache.contains_key(&update.query) {
                if cache_order.len() >= cache_limit {
                    let oldest = cache_order.remove(0);
                    match_cache.remove(&oldest);
                }
                cache_order.push(update.query.clone());
                match_cache.insert(update.query.clone(), (update.candidate_indices.clone(), update.results.clone()));
            }
            if update.error {
                history_load_error = true;
            }
            if update.query == query {
                displayed_matched_indices = update.candidate_indices;
                displayed_results = filter_exact_query_match(&query, update.results);
            }
        }

        let (t_cols, t_lines) = tty_terminal_size(fd, (120, 24));
        term_lines = t_lines;
        let render_width = terminal_safe_render_width(t_cols, anchor_col);
        let query_width = query_text_render_width(render_width, 1);
        let continuation_query_width = terminal_safe_render_width(t_cols, 1);
        let query_rows = build_query_visual_rows(&query, query_width, Some(continuation_query_width));
        let required_query_rows = query_rows.len().max(1);
        let max_panel_rows = (term_lines - anchor_row + 1).max(1);
        let desired_panel_rows = if results_expanded {
            max_panel_rows
        } else {
            min_panel_rows.max(required_query_rows + min_result_rows)
        };

        if desired_panel_rows > max_panel_rows && anchor_row > 1 {
            let extra_rows = (desired_panel_rows - max_panel_rows).min(anchor_row - 1);
            if extra_rows > 0 {
                term_write(fd, &format!("{}{}", move_to(term_lines, 1), "\n".repeat(extra_rows)));
                start_row = start_row.saturating_sub(extra_rows).max(1);
                anchor_row = anchor_row.saturating_sub(extra_rows).max(1);
            }
        }
        panel_rows = desired_panel_rows.min((term_lines - anchor_row + 1).max(1));
        if (term_lines - anchor_row + 1) >= min_panel_rows {
            panel_rows = panel_rows.max(min_panel_rows);
        }

        let (_, _, _, layout_results_visible) = wrapped_query_layout(
            &query,
            cursor_pos,
            query_width,
            panel_rows,
            Some(continuation_query_width),
            Some(&query_rows),
        );
        let visible = results_fitting_rows(layout_results_visible).max(1);

        let mut results;
        if let Some((indices, cached_res)) = match_cache.get(&query) {
            results = filter_exact_query_match(&query, cached_res.clone());
            displayed_matched_indices = indices.clone();
            displayed_results = results.clone();
        } else {
            if queued_search_key.as_deref() != Some(&query) {
                let _ = search_tx.send(Some(SearchRequest {
                    query: query.clone(),
                    candidate_indices: None,
                    cwd: current_cwd_text.clone(),
                }));
                queued_search_key = Some(query.clone());
            }
            results = filter_exact_query_match(&query, displayed_results.clone());
        }

        let runtime_limit = if results.is_empty() { 3 } else { 1 };
        let runtime_cache_key = (query.clone(), cursor_pos);
        let runtime_completions = match runtime_completion_cache.get(&runtime_cache_key) {
            Some(comp) => comp.clone(),
            None => {
                let comp = runtime_completion_matches(
                    &query,
                    cursor_pos,
                    None,
                    &current_cwd_path,
                    positive_int_from_env("ZSH_FLEX_HISTORY_MAX_RETURNED_RESULTS", 100),
                );
                if runtime_completion_cache.len() >= 128 {
                    runtime_completion_cache.clear();
                }
                runtime_completion_cache.insert(runtime_cache_key, comp.clone());
                comp
            }
        };
        results = insert_runtime_completions(results, runtime_completions, runtime_limit);

        if let Some(row_pref) = preferred_runtime_row {
            if row_pref < results.len() && results[row_pref].runtime_completion {
                selected = row_pref;
            }
            preferred_runtime_row = None;
        }

        let status_message = if history_load_error && results.is_empty() { "history load failed" } else { "" };
        let debug_note = if history_client.debug {
            if displayed_matched_indices.is_none() { "no-idx" } else { "idx" }
        } else {
            ""
        };

        if selected >= results.len() {
            selected = results.len().saturating_sub(1);
        }
        if selected < offset {
            offset = selected;
        }
        if selected >= offset + visible {
            offset = selected.saturating_sub(visible) + 1;
        }

        let syntax_tokens = syntax_highlighter.highlight(&query);
        let (qs, _, qru, rv) = draw_panel(
            fd,
            anchor_row,
            anchor_col,
            &query,
            cursor_pos,
            sel_anchor,
            sel_end,
            &results,
            selected,
            offset,
            panel_rows,
            t_cols,
            !skip_previous_cursor_clear,
            status_message,
            debug_note,
            syntax_tokens,
            Some(&query_rows),
            &mut render_state,
            &visual_cursor_bg_style,
            &selection_style,
        );
        query_start = qs;
        query_rows_used = qru;
        last_drawn_panel_rows = qru + rv;
        skip_previous_cursor_clear = false;

        term_write(fd, &move_to(start_row, start_col));

        if !prewarm_directory_started {
            prewarm_directory_started = true;
            let prewarm_cwd = current_cwd_path.clone();
            std::thread::spawn(move || {
                let _ = cached_directory_listing(&prewarm_cwd);
            });
        }

        let input_timeout = if queued_search_key.is_some() {
            Some(Duration::from_millis(30))
        } else {
            None
        };

        let ev = read_key(fd, input_timeout);
        if ev == InputEvent::Timeout {
            continue;
        }

        // prepare_for_keypress
        let reported = query_cursor_position(fd);
        if let Some(rep) = reported {
            if rep != (start_row, start_col) {
                let rel_row = (rep.0 as i64) - (anchor_row as i64);
                let abs_row = query_start as i64 + rel_row;
                let rel_col = (rep.1 as i64) - (if abs_row == 0 { anchor_col as i64 } else { 1 }) - 1;
                if rel_row >= 0 && (rel_row as usize) < query_rows_used {
                    cursor_pos = query_pos_from_visual(
                        &query,
                        query_width,
                        query_start,
                        rel_row as usize,
                        rel_col.max(0) as usize,
                        Some(continuation_query_width),
                    );
                }
                reanchor_from_position(
                    rep,
                    fd,
                    &mut start_row,
                    &mut start_col,
                    &mut anchor_row,
                    &mut anchor_col,
                    &mut panel_rows,
                    &query,
                    cursor_pos,
                    &results,
                    &mut last_refresh_query,
                    &mut last_refresh_results,
                    &mut last_refresh_query_rows,
                );
            } else {
                let log_pos = logical_cursor_terminal_position(fd, &query, cursor_pos, anchor_row, anchor_col, panel_rows);
                term_write(fd, &move_to(log_pos.0, log_pos.1));
            }
        } else {
            let log_pos = logical_cursor_terminal_position(fd, &query, cursor_pos, anchor_row, anchor_col, panel_rows);
            term_write(fd, &move_to(log_pos.0, log_pos.1));
        }

        let query_chars_count = query.chars().count();
        let ev = if matches!(&ev, InputEvent::Right) && cursor_pos == query_chars_count {
            InputEvent::Tab
        } else {
            ev
        };

        match ev {
            InputEvent::Interrupt | InputEvent::Escape => {
                clear_panel_and_restore_cursor(fd, start_row, start_col, anchor_row, anchor_col, inline_with_prompt, true, &mut mouse_enabled, &mut kitty_keyboard_enabled);
                return None;
            }
            InputEvent::Enter => {
                chosen = Some(query);
                break;
            }
            InputEvent::Tab => {
                if selected < results.len() {
                    let selected_result = &results[selected];
                    preferred_runtime_row = if selected_result.runtime_completion { Some(0) } else { None };
                    let (token_start, token_end) = token_bounds(&query, cursor_pos);
                    let (quote, _) = enclosing_quote(query_char_slice(&query, token_start, token_end).as_str());
                    let trailing_text_len = query_chars_count.saturating_sub(token_end);
                    query = selected_result.text.clone();
                    cursor_pos = query.chars().count();
                    if selected_result.runtime_completion {
                        cursor_pos = cursor_pos.saturating_sub(trailing_text_len);
                        if quote.is_some() {
                            cursor_pos = token_start.max(cursor_pos.saturating_sub(1));
                        }
                    } else if matches!(query.chars().last(), Some('\'' | '"')) {
                        cursor_pos = cursor_pos.saturating_sub(1);
                    }
                    if let Some((start, end)) = quoted_value_selection(&query) {
                        sel_anchor = Some(start);
                        sel_end = Some(end);
                        cursor_pos = end;
                    } else {
                        sel_anchor = None;
                        sel_end = None;
                    }
                    sync_mouse_mode(query.chars().count(), &mut mouse_enabled, &mut kitty_keyboard_enabled, fd);
                    if preferred_runtime_row.is_none() {
                        selected = 0;
                    }
                    offset = 0;
                    clear_after_query_suffix(fd, &query, cursor_pos, anchor_row, anchor_col, panel_rows);
                }
            }
            InputEvent::Left => {
                move_cursor(cursor_pos.saturating_sub(1), false, query_chars_count, &mut cursor_pos, &mut sel_anchor, &mut sel_end);
            }
            InputEvent::Right => {
                move_cursor((cursor_pos + 1).min(query_chars_count), false, query_chars_count, &mut cursor_pos, &mut sel_anchor, &mut sel_end);
            }
            InputEvent::ShiftLeft => {
                move_cursor(cursor_pos.saturating_sub(1), true, query_chars_count, &mut cursor_pos, &mut sel_anchor, &mut sel_end);
            }
            InputEvent::ShiftRight => {
                move_cursor((cursor_pos + 1).min(query_chars_count), true, query_chars_count, &mut cursor_pos, &mut sel_anchor, &mut sel_end);
            }
            InputEvent::Home => {
                move_cursor(0, false, query_chars_count, &mut cursor_pos, &mut sel_anchor, &mut sel_end);
            }
            InputEvent::ShiftHome => {
                move_cursor(0, true, query_chars_count, &mut cursor_pos, &mut sel_anchor, &mut sel_end);
            }
            InputEvent::End => {
                move_cursor(query_chars_count, false, query_chars_count, &mut cursor_pos, &mut sel_anchor, &mut sel_end);
            }
            InputEvent::ShiftEnd => {
                move_cursor(query_chars_count, true, query_chars_count, &mut cursor_pos, &mut sel_anchor, &mut sel_end);
            }
            InputEvent::WordLeft => {
                let target = move_word_left(&query, cursor_pos);
                move_cursor(target, false, query_chars_count, &mut cursor_pos, &mut sel_anchor, &mut sel_end);
            }
            InputEvent::WordRight => {
                let target = move_word_right(&query, cursor_pos);
                move_cursor(target, false, query_chars_count, &mut cursor_pos, &mut sel_anchor, &mut sel_end);
            }
            InputEvent::SelectAll => {
                select_all_query(query_chars_count, &mut cursor_pos, &mut sel_anchor, &mut sel_end);
            }
            InputEvent::Up => {
                selected = selected.saturating_sub(1);
            }
            InputEvent::Down => {
                let current_term_lines = tty_terminal_size(fd, (120, 24)).1;
                let available_panel_rows = current_term_lines
                    .saturating_sub(anchor_row)
                    .saturating_add(1);
                if available_panel_rows > last_drawn_panel_rows {
                    results_expanded = true;
                }
                selected = (selected + 1).min(results.len().saturating_sub(1));
            }
            InputEvent::PgUp => {
                selected = selected.saturating_sub(visible);
            }
            InputEvent::PgDn => {
                selected = (selected + visible).min(results.len().saturating_sub(1));
            }
            InputEvent::Backspace => {
                if let Some((s, e)) = selection_bounds(sel_anchor, sel_end) {
                    query = query_splice(&query, s, e, "");
                    cursor_pos = s;
                    sel_anchor = None;
                    sel_end = None;
                } else if cursor_pos > 0 {
                    query = query_splice(&query, cursor_pos - 1, cursor_pos, "");
                    cursor_pos -= 1;
                }
                sync_mouse_mode(query.chars().count(), &mut mouse_enabled, &mut kitty_keyboard_enabled, fd);
                selected = 0;
                offset = 0;
            }
            InputEvent::BackspaceWord => {
                if let Some((s, e)) = selection_bounds(sel_anchor, sel_end) {
                    query = query_splice(&query, s, e, "");
                    cursor_pos = s;
                    sel_anchor = None;
                    sel_end = None;
                } else {
                    let new_pos = move_word_left(&query, cursor_pos);
                    if new_pos < cursor_pos {
                        query = query_splice(&query, new_pos, cursor_pos, "");
                        cursor_pos = new_pos;
                    }
                }
                sync_mouse_mode(query.chars().count(), &mut mouse_enabled, &mut kitty_keyboard_enabled, fd);
                selected = 0;
                offset = 0;
            }
            InputEvent::KillToStart => {
                if let Some((s, e)) = selection_bounds(sel_anchor, sel_end) {
                    query = query_splice(&query, s, e, "");
                    cursor_pos = s;
                } else {
                    query = query_char_slice(&query, cursor_pos, query_chars_count);
                    cursor_pos = 0;
                }
                sel_anchor = None;
                sel_end = None;
                sync_mouse_mode(query.chars().count(), &mut mouse_enabled, &mut kitty_keyboard_enabled, fd);
                selected = 0;
                offset = 0;
            }
            InputEvent::KillToEnd => {
                if let Some((s, e)) = selection_bounds(sel_anchor, sel_end) {
                    query = query_splice(&query, s, e, "");
                    cursor_pos = s;
                } else {
                    query = query_char_slice(&query, 0, cursor_pos);
                }
                sel_anchor = None;
                sel_end = None;
                sync_mouse_mode(query.chars().count(), &mut mouse_enabled, &mut kitty_keyboard_enabled, fd);
                selected = 0;
                offset = 0;
            }
            InputEvent::Delete => {
                if let Some((s, e)) = selection_bounds(sel_anchor, sel_end) {
                    query = query_splice(&query, s, e, "");
                    cursor_pos = s;
                    sel_anchor = None;
                    sel_end = None;
                } else if cursor_pos < query_chars_count {
                    query = query_splice(&query, cursor_pos, cursor_pos + 1, "");
                }
                sync_mouse_mode(query.chars().count(), &mut mouse_enabled, &mut kitty_keyboard_enabled, fd);
                selected = 0;
                offset = 0;
            }
            InputEvent::Char(ch) => {
                if ch == ' ' && query.is_empty() && empty_space_command.is_some() {
                    chosen = empty_space_command;
                    skip_history_record = true;
                    break;
                }
                if let Some((s, e)) = selection_bounds(sel_anchor, sel_end) {
                    query = query_splice(&query, s, e, &ch.to_string());
                    cursor_pos = s + 1;
                    sel_anchor = None;
                    sel_end = None;
                } else {
                    skip_previous_cursor_clear = true;
                    query = query_splice(&query, cursor_pos, cursor_pos, &ch.to_string());
                    cursor_pos += 1;
                }
                sync_mouse_mode(query.chars().count(), &mut mouse_enabled, &mut kitty_keyboard_enabled, fd);
                selected = 0;
                offset = 0;
            }
            InputEvent::Copy => {
                if let Some((s, e)) = selection_bounds(sel_anchor, sel_end) {
                    write_clipboard(&query_char_slice(&query, s, e));
                } else if !query.is_empty() {
                    write_clipboard(&query);
                }
            }
            InputEvent::Paste => {
                let pasted = normalize_pasted_text(&read_clipboard());
                if !pasted.is_empty() {
                    let p_len = pasted.chars().count();
                    if let Some((s, e)) = selection_bounds(sel_anchor, sel_end) {
                        query = query_splice(&query, s, e, &pasted);
                        cursor_pos = s + p_len;
                        sel_anchor = None;
                        sel_end = None;
                    } else {
                        query = query_splice(&query, cursor_pos, cursor_pos, &pasted);
                        cursor_pos += p_len;
                    }
                    sync_mouse_mode(query.chars().count(), &mut mouse_enabled, &mut kitty_keyboard_enabled, fd);
                    selected = 0;
                    offset = 0;
                }
            }
            InputEvent::PasteText(raw_pasted) => {
                let pasted = normalize_pasted_text(&raw_pasted);
                if !pasted.is_empty() {
                    let p_len = pasted.chars().count();
                    if let Some((s, e)) = selection_bounds(sel_anchor, sel_end) {
                        query = query_splice(&query, s, e, &pasted);
                        cursor_pos = s + p_len;
                        sel_anchor = None;
                        sel_end = None;
                    } else {
                        query = query_splice(&query, cursor_pos, cursor_pos, &pasted);
                        cursor_pos += p_len;
                    }
                    sync_mouse_mode(query.chars().count(), &mut mouse_enabled, &mut kitty_keyboard_enabled, fd);
                    selected = 0;
                    offset = 0;
                }
            }
            InputEvent::Mouse { bstate, x: mx, y: my, action } => {
                if bstate & 64 != 0 {
                    if action == 'M' {
                        let wheel_button = bstate & 3;
                        if wheel_button == 0 {
                            selected = selected.saturating_sub(1);
                        } else if wheel_button == 1 {
                            selected = (selected + 1).min(results.len().saturating_sub(1));
                        }
                    }
                    continue;
                }
                let button = bstate & 3;
                let is_motion = (bstate & 32) != 0;
                let is_shift = (bstate & 4) != 0;

                if action == 'm' {
                    if button == 0 || button == 3 {
                        mouse_selecting = false;
                    }
                    continue;
                }
                if action != 'M' {
                    continue;
                }

                if my >= anchor_row && my < (anchor_row + query_rows_used) {
                    let click_row = my - anchor_row;
                    let absolute_query_row = query_start + click_row;
                    let click_col = query_click_visual_col(mx, absolute_query_row, anchor_col);
                    let click_pos = query_pos_from_visual(
                        &query,
                        query_width,
                        query_start,
                        click_row,
                        click_col,
                        Some(continuation_query_width),
                    );

                    if is_motion {
                        if mouse_selecting {
                            move_cursor(click_pos, true, query_chars_count, &mut cursor_pos, &mut sel_anchor, &mut sel_end);
                        }
                        continue;
                    }

                    if button == 0 {
                        let now = Instant::now();
                        let is_same_click_area = now.duration_since(last_left_click_time).as_millis() <= 350
                            && my as i32 == last_left_click_row
                            && (mx as i32 - last_left_click_col).abs() <= 1;

                        if is_same_click_area {
                            left_click_count += 1;
                        } else {
                            left_click_count = 1;
                        }
                        last_left_click_time = now;
                        last_left_click_row = my as i32;
                        last_left_click_col = mx as i32;

                        if left_click_count >= 3 && !query.is_empty() {
                            select_all_query(query_chars_count, &mut cursor_pos, &mut sel_anchor, &mut sel_end);
                            mouse_selecting = false;
                        } else if left_click_count == 2 && !query.is_empty() {
                            let mut left = click_pos;
                            let mut right = click_pos;
                            let chars: Vec<char> = query.chars().collect();
                            if click_pos < chars.len() {
                                let select_ws = chars[click_pos].is_whitespace();
                                while left > 0 && chars[left - 1].is_whitespace() == select_ws {
                                    left -= 1;
                                }
                                right = click_pos + 1;
                                while right < chars.len() && chars[right].is_whitespace() == select_ws {
                                    right += 1;
                                }
                            } else {
                                while left > 0 && !chars[left - 1].is_whitespace() {
                                    left -= 1;
                                }
                            }
                            if left != right {
                                sel_anchor = Some(left);
                                sel_end = Some(right);
                                cursor_pos = right;
                            } else {
                                move_cursor(click_pos, is_shift, query_chars_count, &mut cursor_pos, &mut sel_anchor, &mut sel_end);
                            }
                        } else {
                            move_cursor(click_pos, is_shift, query_chars_count, &mut cursor_pos, &mut sel_anchor, &mut sel_end);
                            mouse_selecting = true;
                        }
                    }
                }
            }
            _ => {}
        }
    }

    clear_panel_and_restore_cursor(fd, start_row, start_col, anchor_row, anchor_col, inline_with_prompt, false, &mut mouse_enabled, &mut kitty_keyboard_enabled);

    let _ = search_tx.send(None);
    chosen.map(|c| (c, skip_history_record))
}
