use unicode_width::UnicodeWidthChar;

pub fn char_display_width(ch: char) -> usize {
    if ch == '\n' {
        return 0;
    }
    if ch == '\t' {
        return 4;
    }
    let codepoint = ch as u32;
    if codepoint < 32 || (0x7F..=0x9F).contains(&codepoint) {
        return 0;
    }
    UnicodeWidthChar::width(ch).unwrap_or(0)
}

pub fn text_display_width(text: &str) -> usize {
    text.chars().map(char_display_width).sum()
}

pub fn truncate_text(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let w = char_display_width(ch);
        if used + w > width {
            break;
        }
        out.push(ch);
        used += w;
    }
    out
}

pub fn query_text_render_width(render_width: usize, lead_cols: usize) -> usize {
    render_width.saturating_sub(lead_cols).max(1)
}

pub fn terminal_safe_render_width(terminal_width: usize, start_col: usize) -> usize {
    let start = start_col.max(1);
    if terminal_width >= start {
        (terminal_width - start + 1).max(1)
    } else {
        1
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryVisualRow {
    pub start: usize,
    pub end: usize,
    pub text: String,
    pub display_width: usize,
}

pub fn build_query_visual_rows(
    query: &str,
    render_width: usize,
    continuation_width: Option<usize>,
) -> Vec<QueryVisualRow> {
    let first_width = render_width.max(1);
    let following_width = continuation_width.unwrap_or(render_width).max(1);
    let mut rows: Vec<QueryVisualRow> = Vec::new();
    let mut start = 0;
    let mut buf = String::new();
    let mut buf_width = 0;

    let chars: Vec<char> = query.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let width = if rows.is_empty() {
            first_width
        } else {
            following_width
        };
        let ch = chars[i];
        if ch == '\n' {
            rows.push(QueryVisualRow {
                start,
                end: i,
                text: std::mem::take(&mut buf),
                display_width: buf_width,
            });
            i += 1;
            start = i;
            buf_width = 0;
            continue;
        }
        let ch_width = char_display_width(ch);
        if ch_width > 0 && !buf.is_empty() && (buf_width + ch_width) > width {
            rows.push(QueryVisualRow {
                start,
                end: i,
                text: std::mem::take(&mut buf),
                display_width: buf_width,
            });
            start = i;
            buf_width = 0;
            continue;
        }
        if ch_width > 0 && buf.is_empty() && ch_width > width {
            rows.push(QueryVisualRow {
                start,
                end: i + 1,
                text: ch.to_string(),
                display_width: width,
            });
            i += 1;
            start = i;
            buf_width = 0;
            continue;
        }
        buf.push(ch);
        buf_width += ch_width;
        i += 1;
    }

    let final_width = if rows.is_empty() {
        first_width
    } else {
        following_width
    };
    rows.push(QueryVisualRow {
        start,
        end: chars.len(),
        text: buf,
        display_width: buf_width,
    });
    if !query.is_empty() && buf_width == final_width {
        rows.push(QueryVisualRow {
            start: chars.len(),
            end: chars.len(),
            text: String::new(),
            display_width: 0,
        });
    }
    rows
}

pub fn query_cursor_visual_position(rows: &[QueryVisualRow], cursor_pos: usize) -> (usize, usize) {
    if rows.is_empty() {
        return (0, 0);
    }
    for (rindex, row) in rows.iter().enumerate() {
        let at_explicit_newline = cursor_pos == row.end
            && rindex + 1 < rows.len()
            && rows[rindex + 1].start == row.end + 1;
        if cursor_pos < row.end
            || (cursor_pos == row.end && (rindex == rows.len() - 1 || at_explicit_newline))
        {
            let offset = cursor_pos.saturating_sub(row.start).min(row.text.chars().count());
            let sub_text: String = row.text.chars().take(offset).collect();
            let col = text_display_width(&sub_text).min(row.display_width);
            return (rindex, col);
        }
    }
    let last = &rows[rows.len() - 1];
    (rows.len() - 1, last.display_width)
}

pub fn query_pos_from_visual(
    query: &str,
    render_width: usize,
    row_start: usize,
    click_row: usize,
    click_col: usize,
    continuation_width: Option<usize>,
) -> usize {
    let rows = build_query_visual_rows(query, render_width, continuation_width);
    if rows.is_empty() {
        return 0;
    }
    let row_index = (row_start + click_row).min(rows.len() - 1);
    let row = &rows[row_index];
    if click_col >= row.display_width {
        return row.end;
    }
    let mut used = 0;
    for (idx, ch) in row.text.chars().enumerate() {
        let w = char_display_width(ch);
        if w == 0 {
            continue;
        }
        if click_col < used + w {
            return row.start + idx;
        }
        used += w;
    }
    row.end
}

pub fn query_click_visual_col(mouse_col: usize, query_row: usize, anchor_col: usize) -> usize {
    let draw_col = if query_row == 0 { anchor_col } else { 1 };
    let lead_cols = if query_row == 0 { 1 } else { 0 };
    mouse_col.saturating_sub(draw_col + lead_cols)
}

pub fn wrapped_query_layout(
    query: &str,
    cursor_pos: usize,
    render_width: usize,
    panel_rows: usize,
    continuation_width: Option<usize>,
    query_rows: Option<&[QueryVisualRow]>,
) -> (usize, usize, usize, usize) {
    let render_width = render_width.max(1);
    let cursor_pos = cursor_pos.min(query.chars().count());
    let query_rows_limit = panel_rows.saturating_sub(1).max(1);
    let owned_rows;
    let rows = match query_rows {
        Some(r) => r,
        None => {
            owned_rows = build_query_visual_rows(query, render_width, continuation_width);
            &owned_rows
        }
    };
    let (cursor_row, _) = query_cursor_visual_position(rows, cursor_pos);
    let query_start = cursor_row.saturating_sub(query_rows_limit.saturating_sub(1));
    let remaining_rows = rows.len().saturating_sub(query_start).max(1);
    let query_rows_used = query_rows_limit.min(remaining_rows);
    let query_view_len = 0;
    let results_visible = panel_rows.saturating_sub(query_rows_used);
    (query_start, query_view_len, query_rows_used, results_visible)
}

pub fn result_row_offset(visible_index: usize) -> usize {
    visible_index.saturating_mul(2).saturating_add(1)
}

pub fn results_fitting_rows(available_rows: usize) -> usize {
    available_rows / 2
}

pub fn selection_bounds(sel_anchor: Option<usize>, sel_end: Option<usize>) -> Option<(usize, usize)> {
    match (sel_anchor, sel_end) {
        (Some(a), Some(e)) if a != e => Some((a.min(e), a.max(e))),
        _ => None,
    }
}

pub fn token_bounds(query: &str, cursor_pos: usize) -> (usize, usize) {
    let chars: Vec<char> = query.chars().collect();
    let cursor = cursor_pos.min(chars.len());
    let mut tokens: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= chars.len() {
            break;
        }
        let start = i;
        let mut quote: Option<char> = None;
        let mut escaped = false;
        while i < chars.len() {
            let ch = chars[i];
            if escaped {
                escaped = false;
                i += 1;
                continue;
            }
            if ch == '\\' {
                escaped = true;
                i += 1;
                continue;
            }
            if let Some(q) = quote {
                if ch == q {
                    quote = None;
                }
                i += 1;
                continue;
            }
            if ch == '\'' || ch == '"' {
                quote = Some(ch);
                i += 1;
                continue;
            }
            if ch.is_whitespace() {
                break;
            }
            i += 1;
        }
        let end = i;
        tokens.push((start, end));
    }
    for &(start, end) in &tokens {
        if start <= cursor && cursor <= end {
            return (start, end);
        }
    }
    (cursor, cursor)
}

pub fn strip_enclosing_quotes(token: &str) -> &str {
    if token.len() >= 2
        && (token.starts_with('\'') && token.ends_with('\'')
            || token.starts_with('"') && token.ends_with('"'))
    {
        return &token[1..token.len() - 1];
    }
    if token.starts_with('\'') || token.starts_with('"') {
        return &token[1..];
    }
    if token.ends_with('\'') || token.ends_with('"') {
        return &token[..token.len() - 1];
    }
    token
}

pub fn enclosing_quote(token: &str) -> (Option<char>, bool) {
    if token.is_empty() {
        return (None, false);
    }
    let first = token.chars().next().unwrap();
    if first != '\'' && first != '"' {
        return (None, false);
    }
    let closes = token.len() > 1 && token.ends_with(first);
    (Some(first), closes)
}

pub fn shell_unescape_fragment(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' && chars.peek().is_some() {
            out.push(chars.next().unwrap());
        } else {
            out.push(ch);
        }
    }
    out
}

pub fn shell_escape_fragment(text: &str) -> String {
    let mut escaped = String::new();
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '.' | '_' | '/' | '~') {
            escaped.push(ch);
        } else {
            escaped.push('\\');
            escaped.push(ch);
        }
    }
    escaped
}

pub fn shell_escape_quoted_fragment(text: &str, quote: char) -> String {
    if quote == '\'' {
        return text.replace('\'', "'\\''");
    }
    let mut escaped = String::new();
    for ch in text.chars() {
        if matches!(ch, '\\' | '"' | '$' | '`') {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

pub fn replace_query_token(query: &str, cursor_pos: usize, replacement: &str) -> String {
    let (start, end) = token_bounds(query, cursor_pos);
    let chars: Vec<char> = query.chars().collect();
    let prefix: String = chars[..start].iter().collect();
    let suffix: String = chars[end..].iter().collect();
    format!("{}{}{}", prefix, replacement, suffix)
}

pub fn query_char_slice(query: &str, start_char: usize, end_char: usize) -> String {
    let chars: Vec<char> = query.chars().collect();
    let s = start_char.min(chars.len());
    let e = end_char.min(chars.len());
    chars[s..e].iter().collect()
}

pub fn query_splice(query: &str, start_char: usize, end_char: usize, replacement: &str) -> String {
    let chars: Vec<char> = query.chars().collect();
    let s = start_char.min(chars.len());
    let e = end_char.min(chars.len());
    let prefix: String = chars[..s].iter().collect();
    let suffix: String = chars[e..].iter().collect();
    format!("{}{}{}", prefix, replacement, suffix)
}

pub fn shell_words_for_matching(text: &str) -> Vec<String> {
    let stripped = text.trim().to_lowercase();
    if stripped.is_empty() {
        return Vec::new();
    }
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;

    for ch in stripped.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' && !in_single {
            escaped = true;
            continue;
        }
        if ch == '\'' && !in_double {
            in_single = !in_single;
            continue;
        }
        if ch == '"' && !in_single {
            in_double = !in_double;
            continue;
        }
        if ch.is_whitespace() && !in_single && !in_double {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            continue;
        }
        current.push(ch);
    }
    if !current.is_empty() {
        words.push(current);
    }
    if words.is_empty() {
        stripped.split_whitespace().map(|s| s.to_string()).collect()
    } else {
        words
    }
}

pub fn move_word_left(query: &str, cursor_pos: usize) -> usize {
    let chars: Vec<char> = query.chars().collect();
    let mut i = cursor_pos.min(chars.len());
    while i > 0 && chars[i - 1].is_whitespace() {
        i -= 1;
    }
    while i > 0 && !chars[i - 1].is_whitespace() {
        i -= 1;
    }
    i
}

pub fn move_word_right(query: &str, cursor_pos: usize) -> usize {
    let chars: Vec<char> = query.chars().collect();
    let mut i = cursor_pos.min(chars.len());
    let n = chars.len();
    while i < n && !chars[i].is_whitespace() {
        i += 1;
    }
    while i < n && chars[i].is_whitespace() {
        i += 1;
    }
    i
}
