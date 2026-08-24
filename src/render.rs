use std::collections::HashMap;
use std::os::unix::io::RawFd;

use crate::layout::*;
use crate::syntax_highlighting::ansi_for_token;
use crate::terminal::*;

pub const DORIC_CURSOR: &str = "#205798";
pub const DORIC_BG_MAIN: &str = "#fcf0e5";
pub const DORIC_FG_MAIN: &str = "#40282e";
pub const DORIC_BORDER: &str = "#c3a8bf";
pub const DORIC_BG_SHADOW_SUBTLE: &str = "#efe4db";
pub const DORIC_FG_SHADOW_SUBTLE: &str = "#8f5854";
pub const DORIC_BG_NEUTRAL: &str = "#e6d5d0";
pub const DORIC_FG_NEUTRAL: &str = "#514250";
pub const DORIC_BG_SHADOW_INTENSE: &str = "#fcb894";
pub const DORIC_FG_SHADOW_INTENSE: &str = "#a02016";
pub const DORIC_BG_ACCENT: &str = "#c8f0e3";
pub const DORIC_FG_ACCENT: &str = "#085078";
pub const DORIC_FG_RED: &str = "#a02610";
pub const DORIC_FG_GREEN: &str = "#006940";
pub const DORIC_FG_YELLOW: &str = "#753800";
pub const DORIC_FG_BLUE: &str = "#183182";
pub const DORIC_FG_MAGENTA: &str = "#820145";
pub const DORIC_FG_CYAN: &str = "#025763";
pub const DORIC_BG_RED: &str = "#ffbca7";
pub const DORIC_BG_GREEN: &str = "#b2efd8";
pub const DORIC_BG_YELLOW: &str = "#e6e294";
pub const DORIC_BG_BLUE: &str = "#baceef";
pub const DORIC_BG_MAGENTA: &str = "#e2c1e0";
pub const DORIC_BG_CYAN: &str = "#c0e6f9";

pub const RESULT_PREFIX_WIDTH: usize = 2;
pub const FIXED_MATCH_TEXT_WIDTH: usize = 3000;
pub const SELECTOR_GLYPH: &str = "●";
pub const FAILED_SELECTOR_GLYPH: &str = "○";
pub const SELECTOR_GLYPH_ENV: &str = "ZSH_FLEX_HISTORY_SELECTOR_GLYPH";
pub const FAILED_SELECTOR_GLYPH_ENV: &str = "ZSH_FLEX_HISTORY_FAILED_SELECTOR_GLYPH";

pub fn fg_code(slot: u8) -> String {
    if slot <= 7 {
        (30 + slot).to_string()
    } else if slot <= 15 {
        (90 + (slot - 8)).to_string()
    } else {
        "39".to_string()
    }
}

pub fn bg_code(slot: u8) -> String {
    if slot <= 7 {
        (40 + slot).to_string()
    } else if slot <= 15 {
        (100 + (slot - 8)).to_string()
    } else {
        "49".to_string()
    }
}

pub fn hex_to_rgb(value: &str) -> Option<(u8, u8, u8)> {
    let s = value.trim_start_matches('#');
    if s.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some((r, g, b))
}

#[derive(Default, Clone)]
pub struct StyleOptions<'a> {
    pub fg: Option<u8>,
    pub bg: Option<u8>,
    pub fg_rgb: Option<&'a str>,
    pub bg_rgb: Option<&'a str>,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
}

pub fn style(opts: StyleOptions) -> String {
    let mut codes = Vec::new();
    if opts.bold {
        codes.push("1".to_string());
    }
    if opts.dim {
        codes.push("2".to_string());
    }
    if opts.italic {
        codes.push("3".to_string());
    }
    if opts.underline {
        codes.push("4".to_string());
    }
    if let Some(fg) = opts.fg {
        codes.push(fg_code(fg));
    }
    if let Some(bg) = opts.bg {
        codes.push(bg_code(bg));
    }
    if let Some(fg_rgb) = opts.fg_rgb {
        if let Some((r, g, b)) = hex_to_rgb(fg_rgb) {
            codes.extend(vec!["38".to_string(), "2".to_string(), r.to_string(), g.to_string(), b.to_string()]);
        }
    }
    if let Some(bg_rgb) = opts.bg_rgb {
        if let Some((r, g, b)) = hex_to_rgb(bg_rgb) {
            codes.extend(vec!["48".to_string(), "2".to_string(), r.to_string(), g.to_string(), b.to_string()]);
        }
    }
    if codes.is_empty() {
        String::new()
    } else {
        format!("\x1b[{}m", codes.join(";"))
    }
}

pub fn ansi_color_from_env(name: &str, default: Option<u8>) -> Option<u8> {
    let raw = std::env::var(name).unwrap_or_default().trim().to_lowercase().replace('_', "-");
    if raw.is_empty() {
        return default;
    }
    if let Ok(val) = raw.parse::<u8>() {
        if val <= 15 {
            return Some(val);
        }
        return default;
    }
    match raw.as_str() {
        "black" => Some(0),
        "red" => Some(1),
        "green" => Some(2),
        "yellow" => Some(3),
        "blue" => Some(4),
        "magenta" | "purple" => Some(5),
        "cyan" => Some(6),
        "white" => Some(7),
        "bright-black" | "gray" | "grey" => Some(8),
        "bright-red" => Some(9),
        "bright-green" => Some(10),
        "bright-yellow" => Some(11),
        "bright-blue" => Some(12),
        "bright-magenta" | "bright-purple" => Some(13),
        "bright-cyan" => Some(14),
        "bright-white" => Some(15),
        _ => default,
    }
}

pub fn positive_int_from_env(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(default)
}

pub fn glyph_from_env(name: &str, default: &str) -> String {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().chars().next())
        .map(|glyph| glyph.to_string())
        .unwrap_or_else(|| default.to_string())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchResult {
    pub text: String,
    pub score: i64,
    pub exact: bool,
    pub recency: i64,
    pub cwd: Option<String>,
    pub text_lower: Option<String>,
    pub runtime_completion: bool,
    pub failed: bool,
    pub words: Vec<String>,
}

pub fn terminal_safe_result_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            if chars.peek() == Some(&']') {
                chars.next();
                while let Some(&c) = chars.peek() {
                    if c == '\x07' {
                        chars.next();
                        break;
                    }
                    if c == '\x1b' {
                        chars.next();
                        if chars.peek() == Some(&'\\') {
                            chars.next();
                        }
                        break;
                    }
                    chars.next();
                }
                continue;
            } else if chars.peek() == Some(&'[') {
                chars.next();
                while let Some(&c) = chars.peek() {
                    if (0x40..=0x7E).contains(&(c as u32)) {
                        chars.next();
                        break;
                    }
                    chars.next();
                }
                continue;
            } else {
                chars.next();
                continue;
            }
        }
        if (ch as u32) < 32 || ((ch as u32) >= 0x7F && (ch as u32) <= 0x9F) {
            out.push(' ');
        } else {
            out.push(ch);
        }
    }
    out
}

pub fn strip_sgr_escapes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            while let Some(&c) = chars.peek() {
                if c == 'm' {
                    chars.next();
                    break;
                }
                chars.next();
            }
            continue;
        }
        out.push(ch);
    }
    out
}

pub fn result_changed_suffix_col(previous: &str, current: &str, anchor_col: usize) -> usize {
    if current.is_empty() {
        return 1;
    }
    let prev_chars: Vec<char> = previous.chars().collect();
    let curr_chars: Vec<char> = current.chars().collect();
    let mut common_len = 0;
    let limit = prev_chars.len().min(curr_chars.len());
    while common_len < limit && prev_chars[common_len] == curr_chars[common_len] {
        common_len += 1;
    }
    let common_prefix: String = curr_chars[..common_len].iter().collect();
    let safe_prefix = terminal_safe_result_text(&common_prefix);
    let result_padding_width = anchor_col.max(1).saturating_sub(2);
    3 + result_padding_width + text_display_width(&safe_prefix)
}

pub fn render_result_line(
    item: &MatchResult,
    selected: bool,
    width: usize,
    _unselected_white: bool,
    suffix_text: &str,
    selector_glyph: &str,
    result_color: Option<u8>,
    runtime_color: Option<u8>,
) -> String {
    let failed_selector_glyph = glyph_from_env(FAILED_SELECTOR_GLYPH_ENV, FAILED_SELECTOR_GLYPH);
    render_result_line_with_glyphs(
        item,
        selected,
        width,
        _unselected_white,
        suffix_text,
        selector_glyph,
        &failed_selector_glyph,
        result_color,
        runtime_color,
    )
}

fn render_result_line_with_glyphs(
    item: &MatchResult,
    selected: bool,
    width: usize,
    _unselected_white: bool,
    suffix_text: &str,
    selector_glyph: &str,
    failed_selector_glyph: &str,
    result_color: Option<u8>,
    runtime_color: Option<u8>,
) -> String {
    if width == 0 {
        return String::new();
    }

    let gutter_width = RESULT_PREFIX_WIDTH;
    let suffix_width = if !suffix_text.is_empty() {
        text_display_width(suffix_text) + 4
    } else {
        0
    };
    let body_width = width.saturating_sub(gutter_width + suffix_width);
    let display_text = terminal_safe_result_text(&item.text);
    let text = truncate_text(&display_text, body_width);

    let normal_style = if item.runtime_completion {
        if selected {
            format!("{}{}", RESET, style(StyleOptions { fg: runtime_color, bold: true, ..Default::default() }))
        } else {
            format!("{}{}", RESET, style(StyleOptions { fg: runtime_color, ..Default::default() }))
        }
    } else if selected {
        format!("{}{}", RESET, style(StyleOptions { fg: result_color, bold: true, ..Default::default() }))
    } else {
        RESET.to_string()
    };

    let selector_style = if item.runtime_completion {
        style(StyleOptions { fg: runtime_color, bold: true, ..Default::default() })
    } else {
        style(StyleOptions { fg: result_color, bold: true, ..Default::default() })
    };

    let selector_source = if item.failed {
        failed_selector_glyph
    } else {
        selector_glyph
    };
    let selector = selector_source.chars().next().unwrap_or('●');

    let gutter = if selected {
        format!("{}{}{} ", selector_style, selector, RESET)
    } else {
        format!("{}{}{} ", RESET, selector, RESET)
    };

    let mut out = Vec::new();
    let mut active_style = String::new();
    for ch in text.chars() {
        if normal_style != active_style {
            out.push(if !normal_style.is_empty() { normal_style.clone() } else { RESET.to_string() });
            active_style = normal_style.clone();
        }
        out.push(ch.to_string());
    }

    if !suffix_text.is_empty() {
        if normal_style != active_style {
            out.push(normal_style);
        }
        out.push(" ".to_string());
        out.push(format!("{}[{}]{}", style(StyleOptions { fg_rgb: Some(DORIC_FG_SHADOW_SUBTLE), ..Default::default() }), suffix_text, RESET));
        out.push(" ".to_string());
    }
    out.push(RESET.to_string());

    format!("{}{}", gutter, out.join(""))
}

#[derive(Default)]
pub struct PanelRenderState {
    pub previous_visual_cursor: Option<(usize, usize)>,
    pub render_line_cache: HashMap<String, String>,
}

#[allow(clippy::too_many_arguments)]
pub fn draw_panel(
    fd: RawFd,
    anchor_row: usize,
    anchor_col: usize,
    query: &str,
    cursor_pos: usize,
    sel_anchor: Option<usize>,
    sel_end: Option<usize>,
    results: &[MatchResult],
    selected: usize,
    offset: usize,
    panel_rows: usize,
    width: usize,
    clear_previous_cursor: bool,
    status_message: &str,
    debug_note: &str,
    syntax_tokens: &[u8],
    query_rows_override: Option<&[QueryVisualRow]>,
    state: &mut PanelRenderState,
    visual_cursor_bg_style: &str,
    selection_style: &str,
) -> (usize, usize, usize, usize) {
    let anchor_col = anchor_col.max(1);
    let render_width = terminal_safe_render_width(width, anchor_col);
    let result_anchor_col = 1;
    let result_padding = " ".repeat(anchor_col.saturating_sub(2));
    let result_render_width = terminal_safe_render_width(width, result_anchor_col + result_padding.len());

    let muted = style(StyleOptions { fg_rgb: Some(DORIC_FG_SHADOW_SUBTLE), ..Default::default() });
    let query_lead_cols = 1;
    let query_width = query_text_render_width(render_width, query_lead_cols);
    let continuation_query_width = terminal_safe_render_width(width, 1);

    let query_chars_count = query.chars().count();
    let cursor_pos = cursor_pos.min(query_chars_count);
    if clear_previous_cursor {
        if let Some((prev_r, prev_c)) = state.previous_visual_cursor {
            term_write(fd, &format!("{}{}{} {}", move_to(prev_r, prev_c), RESET, " ", RESET));
        }
    }

    let owned_rows;
    let query_rows = match query_rows_override {
        Some(r) => r,
        None => {
            owned_rows = build_query_visual_rows(query, query_width, Some(continuation_query_width));
            &owned_rows
        }
    };

    let (query_start, query_view_len, query_rows_used, mut results_visible) = wrapped_query_layout(
        query,
        cursor_pos,
        query_width,
        panel_rows,
        Some(continuation_query_width),
        Some(query_rows),
    );

    if query_rows.len() > 1 {
        results_visible = 0;
    }

    let (cursor_row_abs, _) = query_cursor_visual_position(query_rows, cursor_pos);
    let visible_query_rows = &query_rows[query_start..query_start + query_rows_used];
    let sel = selection_bounds(sel_anchor, sel_end);

    let mut query_lines = Vec::new();
    for (row, vrow) in visible_query_rows.iter().enumerate() {
        let seg_len = vrow.display_width;
        let mut query_parts = vec![RESET.to_string()];
        let mut active_query_style = String::new();
        let mut row_cursor_index: Option<usize> = None;
        if query_start + row == cursor_row_abs {
            row_cursor_index = Some(cursor_pos.saturating_sub(vrow.start).min(vrow.text.chars().count()));
        }

        for (i, ch) in vrow.text.chars().enumerate() {
            let qidx = vrow.start + i;
            let token_kind = if qidx < syntax_tokens.len() { syntax_tokens[qidx] } else { 0 };
            let token_style = ansi_for_token(token_kind);

            if row_cursor_index == Some(i) {
                if !active_query_style.is_empty() {
                    query_parts.push(RESET.to_string());
                    active_query_style.clear();
                }
                query_parts.push(format!("{}{}{}{}", token_style, visual_cursor_bg_style, ch, RESET));
                continue;
            }

            if let Some((sel_s, sel_e)) = sel {
                if sel_s <= qidx && qidx < sel_e {
                    if !active_query_style.is_empty() {
                        query_parts.push(RESET.to_string());
                        active_query_style.clear();
                    }
                    if !token_style.is_empty() {
                        query_parts.push(format!("{}{}{}{}", token_style, selection_style, ch, RESET));
                    } else {
                        query_parts.push(format!("{}{}{}", selection_style, ch, RESET));
                    }
                    continue;
                }
            }

            if token_style != active_query_style {
                query_parts.push(if !token_style.is_empty() { token_style.to_string() } else { RESET.to_string() });
                active_query_style = token_style.to_string();
            }
            query_parts.push(ch.to_string());
        }

        if !active_query_style.is_empty() {
            query_parts.push(RESET.to_string());
        }

        if row_cursor_index == Some(vrow.text.chars().count()) {
            query_parts.push(format!("{} {}", visual_cursor_bg_style, RESET));
        }

        let is_first_query_row = query_start + row == 0;
        let mut query_line = format!("{}{}", if is_first_query_row { " " } else { "" }, query_parts.join(""));
        if is_first_query_row && !debug_note.is_empty() {
            let room = render_width.saturating_sub(seg_len + query_lead_cols);
            if room > 0 {
                let note_text = &debug_note[..debug_note.len().min(room.saturating_sub(1))];
                if !note_text.is_empty() {
                    query_line.push_str(&format!(" {}{}{}", muted, note_text, RESET));
                }
            }
        }
        query_lines.push(query_line);
    }

    let shared_result_width = result_render_width.min(RESULT_PREFIX_WIDTH + FIXED_MATCH_TEXT_WIDTH).max(1);
    let result_color = ansi_color_from_env("ZSH_FLEX_HISTORY_COLOR", None);
    let runtime_color = ansi_color_from_env("ZSH_FLEX_HISTORY_RUNTIME_COLOR", None);
    let selector_glyph = glyph_from_env(SELECTOR_GLYPH_ENV, SELECTOR_GLYPH);
    let failed_selector_glyph = glyph_from_env(FAILED_SELECTOR_GLYPH_ENV, FAILED_SELECTOR_GLYPH);

    let mut result_lines = Vec::new();
    for i in 0..results_visible {
        let idx = offset + i;
        if idx >= results.len() {
            if i == 0 && !status_message.is_empty() {
                let status_style = style(StyleOptions {
                    fg_rgb: Some(DORIC_FG_SHADOW_INTENSE),
                    bg_rgb: Some(DORIC_BG_NEUTRAL),
                    bold: true,
                    ..Default::default()
                });
                result_lines.push(format!("{} {} {}", status_style, status_message, RESET));
            } else {
                result_lines.push(String::new());
            }
            continue;
        }

        let item = &results[idx];
        let is_selected = idx == selected;
        let cache_key = format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{:?}\t{:?}",
            item.text,
            item.runtime_completion,
            item.failed,
            is_selected,
            shared_result_width,
            selector_glyph,
            failed_selector_glyph,
            result_color,
            runtime_color
        );

        let base_line = match state.render_line_cache.get(&cache_key) {
            Some(line) => line.clone(),
            None => {
                let line = render_result_line_with_glyphs(
                    item,
                    is_selected,
                    shared_result_width,
                    true,
                    "",
                    &selector_glyph,
                    &failed_selector_glyph,
                    result_color,
                    runtime_color,
                );
                if state.render_line_cache.len() >= 2048 {
                    state.render_line_cache.clear();
                }
                state.render_line_cache.insert(cache_key, line.clone());
                line
            }
        };
        result_lines.push(format!("{}{}", result_padding, base_line));
    }

    let (final_query_row_abs, final_query_col) = query_cursor_visual_position(query_rows, query_chars_count);
    let final_query_row = final_query_row_abs.saturating_sub(query_start);
    let final_query_draw_col = if final_query_row_abs == 0 { anchor_col } else { result_anchor_col };
    let clear_after_query_col = final_query_draw_col + final_query_col + 1;
    term_write(fd, &format!("{}{}", move_to(anchor_row + final_query_row, clear_after_query_col), CLEAR_TO_END));

    let draw_col_for_row = |row_offset: usize| -> usize {
        if query_start + row_offset == 0 {
            anchor_col
        } else {
            result_anchor_col
        }
    };

    for (i, line) in query_lines.iter().take(query_rows_used).enumerate() {
        let draw_col = draw_col_for_row(i);
        term_write(fd, &format!("{}{}", move_to(anchor_row + i, draw_col), line));
        let plain_line = strip_sgr_escapes(line);
        let clear_col = draw_col + text_display_width(&plain_line);
        if clear_col <= width {
            term_write(fd, &format!("{}{}", move_to(anchor_row + i, clear_col), CLEAR_TO_END));
        }
    }

    let remaining_rows = results_visible;
    for (i, line) in result_lines.iter().take(remaining_rows).enumerate() {
        let result_row = anchor_row + query_rows_used + i;
        term_write(fd, &format!("{}{}", move_to(result_row, result_anchor_col), line));
        let plain_line = strip_sgr_escapes(line);
        let clear_col = result_anchor_col + text_display_width(&plain_line);
        if clear_col <= width {
            term_write(fd, &format!("{}{}", move_to(result_row, clear_col), CLEAR_TO_END));
        }
    }

    // Clear lines below panel
    let (_term_cols, term_lines) = tty_terminal_size(fd, (width, 24));
    let clear_start_row = anchor_row + query_rows_used + remaining_rows;
    for row in clear_start_row..=term_lines {
        term_write(fd, &format!("{}{}", move_to(row, 1), CLEAR_TO_END));
    }

    // Keep hidden terminal cursor synchronized for position query
    let (cursor_row_abs, cursor_col) = query_cursor_visual_position(query_rows, cursor_pos);
    let cursor_row = (cursor_row_abs.saturating_sub(query_start)).min(query_rows_used.saturating_sub(1));
    let cursor_lead_cols = if cursor_row_abs == 0 { query_lead_cols } else { 0 };
    let visual_cursor_col = cursor_col + cursor_lead_cols;
    let cursor_render_width = if cursor_row_abs == 0 { render_width } else { continuation_query_width };
    let final_cursor_col = (cursor_col + cursor_lead_cols).min(cursor_render_width.saturating_sub(1));

    term_write(fd, &move_to(anchor_row + cursor_row, draw_col_for_row(cursor_row) + final_cursor_col));
    state.previous_visual_cursor = Some((anchor_row + cursor_row, draw_col_for_row(cursor_row) + visual_cursor_col));

    (query_start, query_view_len, query_rows_used, results_visible)
}
