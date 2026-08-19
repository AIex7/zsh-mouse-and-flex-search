#!/usr/bin/env python3
"""Interactive zsh history search with Emacs-like flex matching."""

from __future__ import annotations

import os
import json
import queue
import re
import select
import shlex
import shutil
import socket
import sqlite3
import subprocess
import sys
import termios
import threading
import time
import tempfile
import tty
from array import array
from datetime import datetime, timezone
import unicodedata
from argparse import SUPPRESS, ArgumentParser
from dataclasses import dataclass, replace
from pathlib import Path
from typing import Any, List, Optional, Sequence

from .syntax_highlighting import IncrementalHighlighter, ansi_for_token, highlight_tokens

from . import engine
from .engine import *
from .engine import _cursor_color, _cursor_color_rgb


_TERMINAL_OSC_RE = re.compile(r"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)?")
_TERMINAL_CSI_RE = re.compile(r"\x1b\[[0-?]*[ -/]*[@-~]")
_TERMINAL_ESC_RE = re.compile(r"\x1b.", flags=re.DOTALL)
_TERMINAL_CONTROL_RE = re.compile(r"[\x00-\x1f\x7f-\x9f]+")
_SGR_ESCAPE_RE = re.compile(r"\x1b\[[0-9;]*m")

def truncate_text(text: str, width: int) -> str:
    if width <= 0:
        return ""
    out: list[str] = []
    used = 0
    for ch in text:
        w = char_display_width(ch)
        if used + w > width and out:
            break
        if used + w > width:
            break
        out.append(ch)
        used += w
    return "".join(out)


def char_display_width(ch: str) -> int:
    if not ch:
        return 0
    if ch == "\n":
        return 0
    if ch == "\t":
        return 4
    codepoint = ord(ch)
    if codepoint < 32 or (0x7F <= codepoint < 0xA0):
        return 0
    if unicodedata.combining(ch):
        return 0
    if unicodedata.east_asian_width(ch) in ("F", "W"):
        return 2
    return 1


def text_display_width(text: str) -> int:
    return sum(char_display_width(ch) for ch in text)


def query_text_render_width(render_width: int, lead_cols: int = 1) -> int:
    return max(1, render_width - max(0, lead_cols))


def terminal_safe_render_width(terminal_width: int, start_col: int) -> int:
    # Use all columns from the starting column through the terminal edge.
    return max(1, terminal_width - max(1, start_col) + 1)


@dataclass
class QueryVisualRow:
    start: int
    end: int
    text: str
    display_width: int


def build_query_visual_rows(
    query: str,
    render_width: int,
    continuation_width: Optional[int] = None,
) -> list[QueryVisualRow]:
    first_width = max(1, render_width)
    following_width = max(1, continuation_width if continuation_width is not None else render_width)
    rows: list[QueryVisualRow] = []
    start = 0
    buf: list[str] = []
    buf_width = 0
    i = 0
    while i < len(query):
        width = first_width if not rows else following_width
        ch = query[i]
        if ch == "\n":
            rows.append(QueryVisualRow(start=start, end=i, text="".join(buf), display_width=buf_width))
            i += 1
            start = i
            buf = []
            buf_width = 0
            continue
        ch_width = char_display_width(ch)
        if ch_width > 0 and buf and (buf_width + ch_width) > width:
            rows.append(QueryVisualRow(start=start, end=i, text="".join(buf), display_width=buf_width))
            start = i
            buf = []
            buf_width = 0
            continue
        if ch_width > 0 and not buf and ch_width > width:
            rows.append(QueryVisualRow(start=start, end=i + 1, text=ch, display_width=width))
            i += 1
            start = i
            buf = []
            buf_width = 0
            continue
        buf.append(ch)
        buf_width += ch_width
        i += 1
    final_width = first_width if not rows else following_width
    rows.append(QueryVisualRow(start=start, end=len(query), text="".join(buf), display_width=buf_width))
    if query and buf_width == final_width:
        # Once the last character occupies the terminal's final available
        # column, the insertion cursor is physically at column one of the
        # following row. Represent that row explicitly so cursor rendering
        # does not depend on the terminal's pending-wrap state.
        rows.append(QueryVisualRow(start=len(query), end=len(query), text="", display_width=0))
    return rows


def query_cursor_visual_position(rows: list[QueryVisualRow], cursor_pos: int) -> tuple[int, int]:
    if not rows:
        return 0, 0
    for rindex, row in enumerate(rows):
        # A wrapped-row boundary is the first character of the following
        # row. Only end-of-input belongs to the final row.
        if cursor_pos < row.end or (cursor_pos == row.end and rindex == len(rows) - 1):
            offset = max(0, min(cursor_pos - row.start, len(row.text)))
            col = text_display_width(row.text[:offset])
            col = max(0, min(col, row.display_width))
            return rindex, col
    last = rows[-1]
    return len(rows) - 1, last.display_width


def query_pos_from_visual(
    query: str,
    render_width: int,
    row_start: int,
    click_row: int,
    click_col: int,
    continuation_width: Optional[int] = None,
) -> int:
    rows = build_query_visual_rows(query, render_width, continuation_width)
    if not rows:
        return 0
    row_index = max(0, min(row_start + click_row, len(rows) - 1))
    row = rows[row_index]
    col = max(0, click_col)
    if col >= row.display_width:
        return row.end
    used = 0
    for idx, ch in enumerate(row.text):
        w = char_display_width(ch)
        if w <= 0:
            continue
        if col < used + w:
            return row.start + idx
        used += w
    return row.end


def query_click_visual_col(mouse_col: int, query_row: int, anchor_col: int) -> int:
    """Translate a 1-based terminal mouse column into a query-row column."""
    draw_col = anchor_col if query_row == 0 else 1
    lead_cols = 1 if query_row == 0 else 0
    return max(0, mouse_col - draw_col - lead_cols)


def wrapped_query_layout(
    query: str,
    cursor_pos: int,
    render_width: int,
    panel_rows: int,
    continuation_width: Optional[int] = None,
    query_rows: Optional[list[QueryVisualRow]] = None,
) -> tuple[int, int, int, int]:
    render_width = max(1, render_width)
    cursor_pos = max(0, min(cursor_pos, len(query)))
    query_rows_limit = max(1, panel_rows - 1)
    rows = query_rows if query_rows is not None else build_query_visual_rows(query, render_width, continuation_width)
    cursor_row, _cursor_col = query_cursor_visual_position(rows, cursor_pos)
    query_start = max(0, cursor_row - (query_rows_limit - 1))
    query_rows_used = min(query_rows_limit, max(1, len(rows) - query_start))
    query_view_len = 0
    results_visible = max(0, panel_rows - query_rows_used)
    return query_start, query_view_len, query_rows_used, results_visible


def selection_bounds(sel_anchor: Optional[int], sel_end: Optional[int]) -> Optional[tuple[int, int]]:
    if sel_anchor is None or sel_end is None:
        return None
    if sel_anchor == sel_end:
        return None
    return (min(sel_anchor, sel_end), max(sel_anchor, sel_end))


def terminal_safe_result_text(text: str) -> str:
    """Remove terminal control sequences from untrusted history text."""
    # Strip OSC and CSI sequences first, then remove any remaining two-byte
    # ESC sequence (including ESC E / NEL, which advances to a new line).
    text = _TERMINAL_OSC_RE.sub("", text)
    text = _TERMINAL_CSI_RE.sub("", text)
    text = _TERMINAL_ESC_RE.sub("", text)
    return _TERMINAL_CONTROL_RE.sub(" ", text)


def render_result_line(
    item: MatchResult,
    selected: bool,
    width: int,
    *,
    unselected_white: bool = False,
    suffix_text: str = "",
    selector_glyph: str = SELECTOR_GLYPH,
    result_color: Optional[int] = None,
    runtime_color: Optional[int] = None,
) -> str:
    if width <= 0:
        return ""

    gutter_width = RESULT_PREFIX_WIDTH
    suffix_width = text_display_width(suffix_text) + 4 if suffix_text else 0
    body_width = max(0, width - gutter_width - suffix_width)
    display_text = terminal_safe_result_text(item.text)
    text = truncate_text(display_text, body_width)
    if item.runtime_completion:
        if selected:
            normal_style = RESET + style(fg=runtime_color, bold=True)
        else:
            normal_style = RESET + style(fg=runtime_color)
    else:
        if selected:
            normal_style = RESET + style(fg=result_color, bold=True)
        else:
            normal_style = RESET

    if item.runtime_completion:
        selector_style = style(fg=runtime_color, bold=True)
    else:
        selector_style = style(fg=result_color, bold=True)
    selector_source = FAILED_SELECTOR_GLYPH if item.failed else selector_glyph
    selector = selector_source[:1] or SELECTOR_GLYPH
    if selected:
        gutter = f"{selector_style}{selector}{RESET} "
    else:
        gutter = f"{RESET}{selector} "

    out: list[str] = []
    active_style = ""
    for ch in text:
        if normal_style != active_style:
            out.append(normal_style if normal_style else RESET)
            active_style = normal_style
        out.append(ch)
    if suffix_text:
        if normal_style != active_style:
            out.append(normal_style)
        out.append(" ")
        out.append(f"{style(fg_rgb=DORIC['fg_shadow_subtle'])}[{suffix_text}]{RESET}")
        out.append(" ")
    out.append(RESET)
    return gutter + "".join(out)


def draw_panel(
    anchor_row: int,
    anchor_col: int,
    query: str,
    cursor_pos: int,
    sel_anchor: Optional[int],
    sel_end: Optional[int],
    results: list[MatchResult],
    selected: int,
    offset: int,
    panel_rows: int,
    width: int,
    clear_previous_cursor: bool = True,
    status_message: str = "",
    debug_note: str = "",
    total_count: Optional[int] = None,
    syntax_tokens: Optional[list[str]] = None,
    query_rows: Optional[list[QueryVisualRow]] = None,
    render_line_cache: Optional[dict[tuple[object, ...], str]] = None,
) -> tuple[int, int, int, int]:
    anchor_col = max(1, anchor_col)
    render_width = terminal_safe_render_width(width, anchor_col)
    result_anchor_col = 1
    result_render_width = terminal_safe_render_width(width, result_anchor_col)

    def draw_col_for_row(row_offset: int) -> int:
        # ``row_offset`` is relative to the visible query window.  Only the
        # actual first query row follows the prompt; a scrolled-in row is a
        # continuation and must start at column one too.
        if query_start + row_offset == 0:
            return anchor_col
        return result_anchor_col

    muted = style(fg_rgb=DORIC["fg_shadow_subtle"])
    query_lead_cols = 1
    query_width = query_text_render_width(render_width, query_lead_cols)
    continuation_query_width = terminal_safe_render_width(width, 1)

    query_lines: list[str] = []
    result_lines: list[str] = []
    cursor_pos = max(0, min(cursor_pos, len(query)))
    previous_visual_cursor = getattr(draw_panel, "_previous_visual_cursor", None)
    if clear_previous_cursor and previous_visual_cursor is not None:
        term_write(move_to(*previous_visual_cursor) + RESET + " " + RESET)
    query_start, query_view_len, query_rows_used, results_visible = wrapped_query_layout(
        query,
        cursor_pos,
        query_width,
        panel_rows,
        continuation_query_width,
        query_rows,
    )
    if query_rows is None:
        query_rows = build_query_visual_rows(query, query_width, continuation_query_width)
    if len(query_rows) > 1:
        # A wrapped or explicitly multiline query owns the panel; do not
        # render results beneath it.
        results_visible = 0
    cursor_row_abs, _cursor_col_abs = query_cursor_visual_position(query_rows, cursor_pos)
    visible_query_rows = query_rows[query_start : query_start + query_rows_used]
    sel = selection_bounds(sel_anchor, sel_end)
    if syntax_tokens is None:
        syntax_tokens = highlight_tokens(query)
    for row, vrow in enumerate(visible_query_rows):
        seg_len = vrow.display_width
        query_parts: list[str] = [RESET]
        active_query_style = ""
        row_cursor_index: Optional[int] = None
        if query_start + row == cursor_row_abs:
            row_cursor_index = max(0, min(cursor_pos - vrow.start, len(vrow.text)))
        for i, ch in enumerate(vrow.text):
            qidx = vrow.start + i
            token = syntax_tokens[qidx] if qidx < len(syntax_tokens) else "default"
            token_style = ansi_for_token(token)
            if row_cursor_index == i:
                if active_query_style:
                    query_parts.append(RESET)
                    active_query_style = ""
                query_parts.append(f"{token_style}{VISUAL_CURSOR_BG}{ch}{RESET}")
                continue
            if sel and sel[0] <= qidx < sel[1]:
                if active_query_style:
                    query_parts.append(RESET)
                    active_query_style = ""
                if token_style:
                    query_parts.append(f"{QUERY_SELECTION_BG}{token_style}{ch}{RESET}")
                else:
                    query_parts.append(f"{QUERY_SELECTION_BG}{ch}{RESET}")
                continue
            if token_style != active_query_style:
                query_parts.append(token_style if token_style else RESET)
                active_query_style = token_style
            query_parts.append(ch)
        if active_query_style:
            query_parts.append(RESET)
        if row_cursor_index == len(vrow.text):
            query_parts.append(f"{VISUAL_CURSOR_BG} {RESET}")
        # Only the first query row follows the prompt. Wrapped rows start at
        # column one and therefore do not need the prompt lead space.
        is_first_query_row = query_start + row == 0
        query_line = (" " if is_first_query_row else "") + "".join(query_parts)
        if is_first_query_row and debug_note:
            room = max(0, render_width - (seg_len + query_lead_cols))
            if room > 0:
                note_text = debug_note[: max(0, room - 1)]
                if note_text:
                    query_line += f" {muted}{note_text}{RESET}"
        query_lines.append(query_line)

    effective_total = max(len(results), total_count or 0)
    top_remaining = max(0, effective_total - results_visible)
    use_visible_total_for_more = top_remaining <= 97
    shared_result_width = max(1, min(result_render_width, RESULT_PREFIX_WIDTH + FIXED_MATCH_TEXT_WIDTH))
    result_color = ansi_color_from_env("ZSH_FLEX_HISTORY_COLOR", None)
    runtime_color = ansi_color_from_env("ZSH_FLEX_HISTORY_RUNTIME_COLOR", None)
    visible_result_count = min(results_visible, max(0, len(results) - offset))
    for i in range(results_visible):
        idx = offset + i
        if idx >= len(results):
            if i == 0 and status_message:
                result_lines.append(
                    f"{style(fg_rgb=DORIC['fg_shadow_intense'], bg_rgb=DORIC['bg_neutral'], bold=True)} {status_message} {RESET}"
                )
            else:
                result_lines.append("")
            continue
        remaining = max(0, effective_total - (offset + results_visible))
        if use_visible_total_for_more:
            remaining = max(0, len(results) - (offset + results_visible))
        is_last_visible_row = i == (results_visible - 1)
        # more_text = f"{remaining} more" if (is_last_visible_row and remaining > 0) else ""
        item = results[idx]
        is_selected = idx == selected
        cache_key = (
            item.text,
            item.text_lower,
            item.runtime_completion,
            item.failed,
            is_selected,
            shared_result_width,
            SELECTOR_GLYPH,
            result_color,
            runtime_color,
        )
        base_line = render_line_cache.get(cache_key) if render_line_cache is not None else None
        if base_line is None:
            base_line = render_result_line(
                item,
                is_selected,
                shared_result_width,
                unselected_white=True,
                suffix_text="",
                selector_glyph=SELECTOR_GLYPH,
                result_color=result_color,
                runtime_color=runtime_color,
            )
            if render_line_cache is not None:
                if len(render_line_cache) >= 2048:
                    render_line_cache.clear()
                render_line_cache[cache_key] = base_line
        result_lines.append(base_line)

    final_query_row_abs, final_query_col = query_cursor_visual_position(query_rows, len(query))
    final_query_row = final_query_row_abs - query_start
    final_query_draw_col = anchor_col if final_query_row_abs == 0 else result_anchor_col
    clear_after_query_col = final_query_draw_col + final_query_col + 1
    term_write(move_to(anchor_row + final_query_row, clear_after_query_col) + CLEAR_TO_END)

    for i, line in enumerate(query_lines[:query_rows_used]):
        draw_col = draw_col_for_row(i)
        term_write(move_to(anchor_row + i, draw_col) + line)
        plain_line = _SGR_ESCAPE_RE.sub("", line)
        clear_col = draw_col + text_display_width(plain_line)
        if clear_col <= width:
            term_write(move_to(anchor_row + i, clear_col) + CLEAR_TO_END)
    remaining_rows = results_visible
    for i, line in enumerate(result_lines[:remaining_rows]):
        result_row = anchor_row + query_rows_used + i
        term_write(move_to(result_row, result_anchor_col) + line)
        plain_line = _SGR_ESCAPE_RE.sub("", line)
        clear_col = result_anchor_col + text_display_width(plain_line)
        if clear_col <= width:
            term_write(move_to(result_row, clear_col) + CLEAR_TO_END)

    # Remove anything left below the current panel from previous draws.
    try:
        term_lines = tty_terminal_size(engine.TERM_OUT.fileno()).lines
    except (AttributeError, OSError):
        term_lines = shutil.get_terminal_size((width, 24)).lines
    last_result_row = anchor_row + query_rows_used + len(result_lines[:remaining_rows]) - 1
    for row in range(last_result_row + 1, term_lines + 1):
        term_write(move_to(row, 1) + CLEAR_TO_END)

    # Keep the hidden terminal cursor synchronized for the next position query.
    cursor_row_abs, cursor_col = query_cursor_visual_position(query_rows, cursor_pos)
    cursor_row = min(query_rows_used - 1, max(0, cursor_row_abs - query_start))
    cursor_lead_cols = query_lead_cols if cursor_row_abs == 0 else 0
    visual_cursor_col = cursor_col + cursor_lead_cols
    cursor_render_width = render_width if cursor_row_abs == 0 else continuation_query_width
    cursor_col = max(0, min(cursor_col + cursor_lead_cols, cursor_render_width - 1))
    term_write(move_to(anchor_row + cursor_row, draw_col_for_row(cursor_row) + cursor_col))
    draw_panel._previous_visual_cursor = (
        anchor_row + cursor_row,
        draw_col_for_row(cursor_row) + visual_cursor_col,
    )
    term_flush()
    return query_start, query_view_len, query_rows_used, results_visible


def read_key(fd: int, timeout: Optional[float] = 0.1) -> tuple[str, object]:
    def select_input(wait_timeout: Optional[float]) -> bool:
        ready, _, _ = select.select([fd], [], [], wait_timeout)
        return fd in ready

    def read_escape_tail() -> bytes:
        # Read an escape sequence byte-by-byte so we do not over-read into
        # subsequent pasted payload bytes.
        seq = b""
        deadline = time.monotonic() + 0.05
        while time.monotonic() < deadline:
            has_input = select_input(0.01)
            if not has_input:
                if seq:
                    break
                continue
            chunk = os.read(fd, 1)
            if not chunk:
                break
            seq += chunk
            # CSI sequence: ESC [ ... <final>
            if seq.startswith(b"[") and len(seq) >= 2 and seq[-1:] and (64 <= seq[-1] <= 126):
                break
            # SS3 sequence: ESC O <final>
            if seq.startswith(b"O") and len(seq) >= 2:
                break
            # Alt-modified key (ESC + single byte).
            if not seq.startswith((b"[", b"O")) and len(seq) >= 1:
                break
        return seq

    def parse_csi_key(full: bytes) -> Optional[tuple[str, object]]:
        m_u = re.fullmatch(rb"\x1b\[(\d+)(?:;(\d+))?u", full)
        if m_u:
            codepoint = int(m_u.group(1))
            mod = int(m_u.group(2) or b"1")
            shift = (mod - 1) & 1
            ctrl = (mod - 1) & 4
            alt = (mod - 1) & 2
            super_key = (mod - 1) & 8

            if codepoint == 13:
                return "enter", None
            if codepoint == 9:
                return "tab", None
            if codepoint in (8, 127):
                if ctrl:
                    return "backspace_word", None
                return "backspace", None
            if codepoint == 27:
                return "quit", None
            if codepoint == 1 and ctrl:
                return "home", None
            if codepoint == 5 and ctrl:
                return "end", None
            if codepoint == 11 and ctrl:
                return "kill_to_end", None
            if codepoint == 21 and ctrl:
                return "kill_to_start", None
            if codepoint == 23 and ctrl:
                return "backspace_word", None
            if codepoint == 98 and alt:
                return "word_left", None
            if codepoint == 102 and alt:
                return "word_right", None
            if codepoint in (65, 97) and alt:
                return "select_all", None
            if codepoint in (67, 99) and (alt or super_key):
                return "copy", None
            if codepoint in (86, 118) and (alt or super_key):
                return "paste", None
            if 32 <= codepoint < 127:
                return "char", chr(codepoint)

        # Handle modified cursor keys in CSI-u style (e.g. ESC [ 1 ; 2 D).
        m = re.fullmatch(rb"\x1b\[(?:1;)?(\d+)([ABCDHF])", full)
        if m:
            mod = int(m.group(1))
            key = m.group(2)
            if mod in (1,):
                if key == b"D":
                    return "left", None
                if key == b"C":
                    return "right", None
                if key == b"H":
                    return "home", None
                if key == b"F":
                    return "end", None
            if mod == 2:
                if key == b"D":
                    return "shift_left", None
                if key == b"C":
                    return "shift_right", None
                if key == b"H":
                    return "shift_home", None
                if key == b"F":
                    return "shift_end", None
            if mod == 5:
                if key == b"D":
                    return "word_left", None
                if key == b"C":
                    return "word_right", None
        # xterm/kitty ctrl+arrow variants.
        if full in (b"\x1b[1;5D", b"\x1b[5D"):
            return "word_left", None
        if full in (b"\x1b[1;5C", b"\x1b[5C"):
            return "word_right", None
        if full in (b"\x1b[1;2D",):
            return "shift_left", None
        if full in (b"\x1b[1;2C",):
            return "shift_right", None
        if full in (b"\x1b[1;2H",):
            return "shift_home", None
        if full in (b"\x1b[1;2F",):
            return "shift_end", None
        return None

    def read_pending_burst(initial: bytes = b"") -> str:
        buf = bytearray(initial)
        deadline = time.monotonic() + 0.3
        while time.monotonic() < deadline:
            ready, _, _ = select.select([fd], [], [], 0.015)
            if not ready:
                break
            chunk = os.read(fd, 4096)
            if not chunk:
                break
            buf.extend(chunk)
            if len(buf) >= 1_000_000:
                break
        return bytes(buf).decode("utf-8", errors="replace")

    def read_utf8_char(first_byte: int) -> str:
        if first_byte < 0x80:
            return chr(first_byte)
        need = 0
        if (first_byte & 0xE0) == 0xC0:
            need = 2
        elif (first_byte & 0xF0) == 0xE0:
            need = 3
        elif (first_byte & 0xF8) == 0xF0:
            need = 4
        if need == 0:
            return bytes((first_byte,)).decode("utf-8", errors="replace")
        buf = bytearray((first_byte,))
        deadline = time.monotonic() + 0.03
        while len(buf) < need and time.monotonic() < deadline:
            ready, _, _ = select.select([fd], [], [], 0.005)
            if not ready:
                break
            chunk = os.read(fd, 1)
            if not chunk:
                break
            buf.extend(chunk)
        return bytes(buf).decode("utf-8", errors="replace")

    while True:
        has_input = select_input(timeout)
        if not has_input:
            return "timeout", None
        data = os.read(fd, 1)
        if not data:
            continue

        ch = data[0]
        if ch == 3:
            return "interrupt", None
        if ch == 1:
            return "home", None
        if ch == 5:
            return "end", None
        if ch in (10, 13):
            # If newline arrives with queued bytes, treat as pasted content
            # instead of immediate submit.
            queued, _, _ = select.select([fd], [], [], 0)
            if queued:
                return "paste_text", read_pending_burst(b"\n")
            return "enter", None
        if ch == 9:
            return "tab", None
        if ch in (8, 127):
            return "backspace", None
        if ch == 23:
            return "backspace_word", None
        if ch == 21:
            return "kill_to_start", None
        if ch == 11:
            return "kill_to_end", None
        if ch == 27:
            seq = read_escape_tail()
            full = b"\x1b" + seq
            if full == b"\x1b":
                return "escape", None
            if full in (b"\x1b[A",):
                return "up", None
            if full in (b"\x1b[B",):
                return "down", None
            if full in (b"\x1b[C",):
                return "right", None
            if full in (b"\x1b[D",):
                return "left", None
            if full in (b"\x1b[H", b"\x1b[1~", b"\x1bOH"):
                return "home", None
            if full in (b"\x1b[F", b"\x1b[4~", b"\x1bOF"):
                return "end", None
            if full in (b"\x1b[3~",):
                return "delete", None
            if full in (b"\x1b[5~",):
                return "pgup", None
            if full in (b"\x1b[6~",):
                return "pgdn", None
            parsed = parse_csi_key(full)
            if parsed is not None:
                return parsed

            m = re.match(rb"\x1b\[<(\d+);(\d+);(\d+)([mM])", full)
            if m:
                bstate = int(m.group(1))
                x = int(m.group(2))
                y = int(m.group(3))
                action = m.group(4).decode("ascii")
                return "mouse", (bstate, x, y, action)
            if full in (b"\x1bb", b"\x1b[1;3D"):
                return "word_left", None
            if full in (b"\x1bf", b"\x1b[1;3C"):
                return "word_right", None
            if full in (b"\x1ba", b"\x1bA"):
                return "select_all", None
            if full in (b"\x1bc", b"\x1bC"):
                return "copy", None
            if full in (b"\x1bv", b"\x1bV"):
                return "paste", None
            continue
        if ch >= 32:
            queued, _, _ = select.select([fd], [], [], 0)
            if queued:
                burst = read_pending_burst(bytes((ch,)))
                if len(burst) > 1 or "\n" in burst:
                    return "paste_text", burst
                return "char", burst
            return "char", read_utf8_char(ch)


def move_word_left(query: str, cursor_pos: int) -> int:
    i = max(0, min(cursor_pos, len(query)))
    while i > 0 and query[i - 1].isspace():
        i -= 1
    while i > 0 and not query[i - 1].isspace():
        i -= 1
    return i


def move_word_right(query: str, cursor_pos: int) -> int:
    i = max(0, min(cursor_pos, len(query)))
    n = len(query)
    while i < n and not query[i].isspace():
        i += 1
    while i < n and query[i].isspace():
        i += 1
    return i


def run(
    *,
    inline_with_prompt: bool = False,
    history_client: HistoryDaemonClient,
    empty_space_command: Optional[str] = None,
) -> Optional[tuple[str, bool]]:
    global VISUAL_CURSOR_BG
    tty_in_file = None
    tty_out_file = None
    current_cwd_text = normalize_cwd_value(os.getcwd())
    current_cwd_path = Path(current_cwd_text)
    startup_entries = cached_directory_listing(current_cwd_path) or ()
    fd: Optional[int] = None
    for tty_path in ("/dev/tty", os.ctermid()):
        try:
            tty_in_file = open(tty_path, "r", encoding="utf-8", buffering=1)
            tty_out_file = open(tty_path, "w", encoding="utf-8", buffering=1)
            candidate_fd = tty_in_file.fileno()
            if os.isatty(candidate_fd):
                fd = candidate_fd
                engine.TERM_OUT = tty_out_file
                break
            tty_in_file.close()
            tty_out_file.close()
            tty_in_file = None
            tty_out_file = None
        except OSError:
            if tty_in_file is not None:
                tty_in_file.close()
                tty_in_file = None
            if tty_out_file is not None:
                tty_out_file.close()
                tty_out_file = None

    if fd is None:
        candidate_fd = sys.stdin.fileno()
        if os.isatty(candidate_fd):
            fd = candidate_fd
            engine.TERM_OUT = sys.stdout

    if fd is None:
        print("zsh_flex_history: no usable TTY available for interactive mode", file=sys.stderr)
        return None
    min_result_rows = 3
    min_panel_rows = 1 + min_result_rows
    try:
        with RawTerminal(fd) as rt:
            # Respect an explicit color override. Otherwise mirror the
            # terminal's real cursor color when it answers OSC 12.
            if _cursor_color is None and _cursor_color_rgb is None:
                cursor_color = query_cursor_color(fd)
                if cursor_color is not None:
                    VISUAL_CURSOR_BG = style(fg_rgb=DORIC["fg_main"], bg_rgb=cursor_color)
            term_size = tty_terminal_size(fd)
            term_lines = term_size.lines
            pos = query_cursor_position(fd)
            if pos is None:
                start_row = max(1, term_lines - 1)
                start_col = 1
            else:
                start_row = pos[0]
                start_col = pos[1]
            # Keep all row math within the visible terminal bounds even if a
            # terminal reports a transient cursor value during startup.
            start_row = max(1, min(start_row, term_lines))
            space_below = max(0, term_lines - start_row)
            # If there is no room to draw result rows below the prompt area,
            # reserve lines by scrolling a small amount.
            if inline_with_prompt:
                required_below = max(0, min_panel_rows - 1)
            else:
                required_below = min_panel_rows
            scroll_rows = max(0, required_below - space_below)
            if scroll_rows > 0:
                term_write(move_to(term_lines, 1) + ("\n" * scroll_rows))
                term_flush()
                start_row = max(1, start_row - scroll_rows)
                space_below = max(0, term_lines - start_row)
            initial_cursor_row = start_row
            initial_cursor_col = start_col

            # For print-only mode, anchor on the prompt row itself so query
            # input starts on the same line as the prompt.
            # Otherwise, use the row below the prompt when possible.
            if inline_with_prompt:
                anchor_row = max(1, start_row)
                anchor_col = max(1, start_col - 1)
                panel_rows = max(1, term_lines - anchor_row + 1)
            elif space_below >= 1:
                anchor_row = start_row + 1
                anchor_col = 1
                panel_rows = max(1, space_below)
            else:
                anchor_row = max(1, start_row)
                anchor_col = 1
                panel_rows = max(1, term_lines - anchor_row + 1)
            term_write(move_to(anchor_row, anchor_col))
            term_flush()

            query = ""
            syntax_highlighter = IncrementalHighlighter()
            last_refresh_query: Optional[str] = None
            last_refresh_results: list[str] = []
            last_refresh_query_rows = 1
            cursor_pos = 0
            skip_history_record = False
            sel_anchor: Optional[int] = None
            sel_end: Optional[int] = None
            selected = 0
            offset = 0
            chosen: Optional[str] = None
            query_start = 0
            query_rows_used = 1
            results_visible = max(1, panel_rows - 1)
            render_width = 1
            initial_matched_indices: Optional[list[int]] = None
            initial_matched_count: Optional[int] = None
            loaded = history_client.search_history(
                "",
                limit=MAX_RETURNED_RESULTS,
                cwd=current_cwd_text,
            )
            if loaded is None:
                initial_results = []
                history_load_error = True
            else:
                history_matches, initial_matched_indices, initial_matched_count = loaded
                initial_results = history_matches
                history_load_error = False
            last_query = ""
            last_matched_indices = initial_matched_indices
            initial_total_count = max(len(initial_results), initial_matched_count or 0)
            match_cache: dict[str, tuple[Optional[array], list[MatchResult], Optional[int], int]] = {
                "": (
                    array("I", initial_matched_indices) if initial_matched_indices is not None else None,
                    initial_results,
                    initial_matched_count,
                    initial_total_count,
                )
            }
            cache_order: list[str] = [""]
            cache_limit = 128
            displayed_results = initial_results
            displayed_matched_indices = initial_matched_indices
            displayed_matched_count = initial_matched_count
            displayed_total_count = initial_total_count
            mouse_selecting = False
            mouse_enabled = False
            kitty_keyboard_enabled = False
            kitty_keyboard_supported = supports_kitty_keyboard_protocol()
            last_left_click_time = 0.0
            last_left_click_row = -1
            last_left_click_col = -1
            left_click_count = 0
            last_drawn_panel_rows = panel_rows
            search_requests: queue.Queue[Optional[tuple[str, Optional[Sequence[int]], str]]] = queue.Queue()
            search_updates: queue.Queue[
                tuple[str, Optional[list[int]], list[MatchResult], Optional[int], int, bool]
            ] = queue.Queue()
            search_stop = threading.Event()
            queued_search_key: Optional[str] = None
            preferred_runtime_row: Optional[int] = None
            runtime_completion_cache: dict[tuple[str, int], list[MatchResult]] = {}
            render_line_cache: dict[tuple[object, ...], str] = {}

            def run_search_request(
                query_text: str,
                candidate_indices: Optional[Sequence[int]],
                cwd_text: str,
            ) -> tuple[Optional[list[int]], list[MatchResult], Optional[int], int, bool]:
                search_error = False
                remote = history_client.search_history(
                    query_text,
                    candidate_indices=candidate_indices,
                    limit=MAX_RETURNED_RESULTS,
                    cwd=cwd_text,
                )
                if remote is None:
                    history_results = []
                    matched_indices = None
                    matched_count = None
                    search_error = True
                else:
                    history_results, matched_indices, matched_count = remote
                # The daemon has already applied this ordering and limit in Rust.
                resolved_results = history_results
                total_count = max(len(resolved_results), matched_count or 0)
                return matched_indices, resolved_results, matched_count, total_count, search_error

            def search_worker() -> None:
                while True:
                    request = search_requests.get()
                    if request is None:
                        break
                    query_text, candidate_indices, cwd_text = request
                    matched_indices, resolved_results, matched_count, total_count, search_error = run_search_request(
                        query_text,
                        candidate_indices,
                        cwd_text,
                    )
                    search_updates.put(
                        (query_text, matched_indices, resolved_results, matched_count, total_count, search_error)
                    )

            search_thread = threading.Thread(target=search_worker, daemon=True)
            search_thread.start()

            def reanchor_from_position(pos: tuple[int, int]) -> None:
                nonlocal start_row, start_col, anchor_row, anchor_col, panel_rows, last_drawn_panel_rows
                nonlocal initial_cursor_row, initial_cursor_col, last_refresh_query, last_refresh_results, last_refresh_query_rows

                term_size = tty_terminal_size(fd)
                term_lines = term_size.lines
                next_start_row = pos[0]
                next_start_col = pos[1]

                next_start_row = max(1, min(next_start_row, term_lines))
                space_below = max(0, term_lines - next_start_row)
                if inline_with_prompt:
                    required_below = max(0, min_panel_rows - 1)
                else:
                    required_below = min_panel_rows
                scroll_rows = max(0, required_below - space_below)
                if scroll_rows > 0:
                    term_write(move_to(term_lines, 1) + ("\n" * scroll_rows))
                    term_flush()
                    next_start_row = max(1, next_start_row - scroll_rows)
                    space_below = max(0, term_lines - next_start_row)

                if inline_with_prompt:
                    next_anchor_row = max(1, next_start_row)
                    next_anchor_col = max(1, next_start_col - 1)
                    next_panel_rows = max(1, term_lines - next_anchor_row + 1)
                elif space_below >= 1:
                    next_anchor_row = next_start_row + 1
                    next_anchor_col = 1
                    next_panel_rows = max(1, space_below)
                else:
                    next_anchor_row = max(1, next_start_row)
                    next_anchor_col = 1
                    next_panel_rows = max(1, term_lines - next_anchor_row + 1)

                start_row = next_start_row
                start_col = next_start_col
                initial_cursor_row = start_row
                initial_cursor_col = start_col
                anchor_row = next_anchor_row
                anchor_col = next_anchor_col
                panel_rows = next_panel_rows
                last_drawn_panel_rows = panel_rows

                render_width = terminal_safe_render_width(term_size.columns, next_anchor_col)
                query_width = query_text_render_width(render_width)
                continuation_query_width = terminal_safe_render_width(term_size.columns, 1)
                query_start, _, query_rows_used, _ = wrapped_query_layout(
                    query,
                    cursor_pos,
                    query_width,
                    next_panel_rows,
                    continuation_query_width,
                )
                current_results = [item.text for item in results[:3]]
                if last_refresh_query != query:
                    query_rows = build_query_visual_rows(query, query_width, continuation_query_width)
                    common_length = 0
                    if last_refresh_query is not None:
                        common_limit = min(len(last_refresh_query), len(query))
                        while (
                            common_length < common_limit
                            and last_refresh_query[common_length] == query[common_length]
                        ):
                            common_length += 1
                    clear_row_abs, clear_col = query_cursor_visual_position(query_rows, common_length)
                    clear_row = max(0, clear_row_abs - query_start)
                    clear_col = (
                        next_anchor_col if clear_row_abs == 0 else 1
                    ) + clear_col + 1
                    for row in range(
                        next_anchor_row + clear_row,
                        next_anchor_row + max(last_refresh_query_rows, query_rows_used),
                    ):
                        term_write(
                            move_to(
                                row,
                                clear_col if row == next_anchor_row + clear_row else 1,
                            )
                            + CLEAR_TO_END
                        )
                    last_refresh_query = query
                    last_refresh_query_rows = query_rows_used
                query_rows = build_query_visual_rows(query, query_width, continuation_query_width)
                last_row_abs, last_col = query_cursor_visual_position(query_rows, len(query))
                last_row = max(0, last_row_abs - query_start)
                last_row_col = (
                    next_anchor_col if last_row_abs == 0 else 1
                ) + last_col + 1
                term_write(move_to(next_anchor_row + last_row, last_row_col) + CLEAR_TO_END)
                for result_index in range(max(len(last_refresh_results), len(current_results))):
                    previous_result = (
                        last_refresh_results[result_index]
                        if result_index < len(last_refresh_results)
                        else ""
                    )
                    current_result = (
                        current_results[result_index] if result_index < len(current_results) else ""
                    )
                    common_length = 0
                    common_limit = min(len(previous_result), len(current_result))
                    while (
                        common_length < common_limit
                        and previous_result[common_length] == current_result[common_length]
                    ):
                        common_length += 1
                    if previous_result != current_result:
                        result_row = next_anchor_row + query_rows_used + result_index
                        clear_result_col = 1 if not current_result else 3 + common_length
                        term_write(move_to(result_row, clear_result_col) + CLEAR_TO_END)
                last_refresh_results = current_results
                clear_after_results_row = next_anchor_row + query_rows_used + 3
                for row in range(clear_after_results_row, term_lines + 1):
                    term_write(move_to(row, 1) + CLEAR_TO_END)
                term_write(move_to(anchor_row, anchor_col))
                term_flush()

            def logical_cursor_terminal_position() -> tuple[int, int]:
                term_size = tty_terminal_size(fd)
                render_width = terminal_safe_render_width(term_size.columns, anchor_col)
                query_width = query_text_render_width(render_width)
                continuation_query_width = terminal_safe_render_width(term_size.columns, 1)
                current_query_start, _, _, _ = wrapped_query_layout(
                    query,
                    cursor_pos,
                    query_width,
                    panel_rows,
                    continuation_query_width,
                )
                query_rows = build_query_visual_rows(query, query_width, continuation_query_width)
                cursor_row_abs, cursor_col = query_cursor_visual_position(query_rows, cursor_pos)
                cursor_row = max(0, cursor_row_abs - current_query_start)
                draw_col = anchor_col if cursor_row_abs == 0 else 1
                return anchor_row + cursor_row, draw_col + cursor_col

            def prepare_for_keypress() -> None:
                nonlocal cursor_pos, start_row, start_col

                reported = query_cursor_position(fd)
                if reported is not None and reported != (start_row, start_col):
                    term_size = tty_terminal_size(fd)
                    render_width = terminal_safe_render_width(term_size.columns, anchor_col)
                    query_width = query_text_render_width(render_width)
                    continuation_query_width = terminal_safe_render_width(term_size.columns, 1)
                    relative_row = reported[0] - anchor_row
                    absolute_row = query_start + relative_row
                    relative_col = reported[1] - (
                        anchor_col if absolute_row == 0 else 1
                    ) - 1
                    if 0 <= relative_row < query_rows_used:
                        cursor_pos = query_pos_from_visual(
                            query,
                            query_width,
                            query_start,
                            relative_row,
                            max(0, relative_col),
                            continuation_query_width,
                        )
                    reanchor_from_position(reported)
                else:
                    term_write(move_to(*logical_cursor_terminal_position()))
                    term_flush()

            def clear_after_query_suffix() -> None:
                term_size = tty_terminal_size(fd)
                render_width = terminal_safe_render_width(term_size.columns, anchor_col)
                query_width = query_text_render_width(render_width)
                continuation_query_width = terminal_safe_render_width(term_size.columns, 1)
                query_start, _, _, _ = wrapped_query_layout(
                    query,
                    cursor_pos,
                    query_width,
                    panel_rows,
                    continuation_query_width,
                )
                query_rows = build_query_visual_rows(query, query_width, continuation_query_width)
                row_abs, col = query_cursor_visual_position(query_rows, len(query))
                row = max(0, row_abs - query_start)
                draw_col = anchor_col if row_abs == 0 else 1
                term_write(move_to(anchor_row + row, draw_col + col + 2) + CLEAR_TO_END)
                term_flush()

            def clear_panel_display() -> None:
                term_lines = tty_terminal_size(fd).lines
                clear_col = anchor_col if inline_with_prompt else 1
                for row in range(anchor_row, term_lines + 1):
                    term_write(move_to(row, clear_col if row == anchor_row else 1) + CLEAR_TO_END)

            def clear_panel_and_restore_cursor(*, clear_display: bool = False) -> None:
                nonlocal mouse_enabled, mouse_selecting, kitty_keyboard_enabled
                if kitty_keyboard_enabled:
                    term_write(DISABLE_KITTY_KEYBOARD)
                    kitty_keyboard_enabled = False
                if mouse_enabled:
                    term_write(DISABLE_MOUSE)
                    mouse_enabled = False
                    mouse_selecting = False
                if clear_display:
                    clear_panel_display()
                draw_panel._previous_visual_cursor = None
                # Restore cursor to the exact prompt position captured at invocation start.
                term_write(move_to(start_row, start_col))
                term_flush()

            def clear_selection() -> None:
                nonlocal sel_anchor, sel_end
                sel_anchor = None
                sel_end = None

            def move_cursor(new_pos: int, *, select_mode: bool = False) -> None:
                nonlocal cursor_pos, sel_anchor, sel_end
                new_pos = max(0, min(new_pos, len(query)))
                if select_mode:
                    if sel_anchor is None:
                        sel_anchor = cursor_pos
                    cursor_pos = new_pos
                    sel_end = cursor_pos
                    if sel_anchor == sel_end:
                        sel_anchor = None
                        sel_end = None
                    return
                cursor_pos = new_pos
                clear_selection()

            def sync_mouse_mode() -> None:
                nonlocal mouse_enabled, mouse_selecting, kitty_keyboard_enabled
                should_enable = len(query) > 0
                if should_enable and not mouse_enabled:
                    term_write(ENABLE_MOUSE)
                    if kitty_keyboard_supported and not kitty_keyboard_enabled:
                        term_write(ENABLE_KITTY_KEYBOARD)
                        kitty_keyboard_enabled = True
                    term_flush()
                    mouse_enabled = True
                elif not should_enable and mouse_enabled:
                    term_write(DISABLE_MOUSE)
                    if kitty_keyboard_enabled:
                        term_write(DISABLE_KITTY_KEYBOARD)
                        kitty_keyboard_enabled = False
                    term_flush()
                    mouse_enabled = False
                    mouse_selecting = False

            def select_all_query() -> None:
                nonlocal sel_anchor, sel_end, cursor_pos
                if not query:
                    clear_selection()
                    return
                sel_anchor = 0
                sel_end = len(query)
                cursor_pos = len(query)

            def cache_put(
                key: str,
                indices: Optional[list[int] | array],
                cached_results: list[MatchResult],
                matched_count: Optional[int],
                total_count: int,
            ) -> None:
                if key in match_cache:
                    return
                if len(cache_order) >= cache_limit:
                    oldest = cache_order.pop(0)
                    match_cache.pop(oldest, None)
                cache_order.append(key)
                packed_indices = array("I", indices) if indices is not None else None
                match_cache[key] = (packed_indices, cached_results, matched_count, total_count)

            try:
                skip_previous_cursor_clear = False
                while True:
                    # Each completed keypress leaves the physical cursor at
                    # the prompt start. Restore that invariant before doing
                    # any work for the next event.
                    term_write(move_to(start_row, start_col))
                    term_flush()
                    while True:
                        try:
                            (
                                result_query,
                                result_indices,
                                result_results,
                                result_count,
                                result_total,
                                result_error,
                            ) = search_updates.get_nowait()
                        except queue.Empty:
                            break
                        queued_search_key = None if queued_search_key == result_query else queued_search_key
                        cache_put(result_query, result_indices, result_results, result_count, result_total)
                        if result_error:
                            history_load_error = True
                        if result_query == query:
                            displayed_matched_indices = result_indices
                            displayed_results = filter_exact_query_match(query, result_results)
                            displayed_matched_count = result_count
                            displayed_total_count = result_total

                    pending_event: Optional[tuple[str, object]] = None
                    term_size = tty_terminal_size(fd)
                    width = term_size.columns
                    term_lines = term_size.lines
                    render_width = terminal_safe_render_width(width, anchor_col)
                    query_width = query_text_render_width(render_width)
                    continuation_query_width = terminal_safe_render_width(width, 1)
                    query_rows = build_query_visual_rows(query, query_width, continuation_query_width)
                    required_query_rows = max(
                        1,
                        len(query_rows),
                    )
                    desired_panel_rows = max(min_panel_rows, required_query_rows + min_result_rows)
                    max_panel_rows = max(1, term_lines - anchor_row + 1)
                    if desired_panel_rows > max_panel_rows and anchor_row > 1:
                        extra_rows = min(desired_panel_rows - max_panel_rows, anchor_row - 1)
                        if extra_rows > 0:
                            term_write(move_to(term_lines, 1) + ("\n" * extra_rows))
                            term_flush()
                            start_row = max(1, start_row - extra_rows)
                            initial_cursor_row = start_row
                            initial_cursor_col = start_col
                            anchor_row = max(1, anchor_row - extra_rows)
                            max_panel_rows = max(1, term_lines - anchor_row + 1)
                    panel_rows = min(desired_panel_rows, max_panel_rows)
                    if max_panel_rows >= min_panel_rows:
                        panel_rows = max(min_panel_rows, panel_rows)

                    _qs, _qvl, _qru, layout_results_visible = wrapped_query_layout(
                        query,
                        cursor_pos,
                        query_width,
                        panel_rows,
                        continuation_query_width,
                        query_rows,
                    )
                    visible = max(1, layout_results_visible)
                    cache_key = query
                    if cache_key in match_cache:
                        matched_indices, results, matched_count, total_count = match_cache[cache_key]
                        results = filter_exact_query_match(query, results)
                        displayed_matched_indices = matched_indices
                        displayed_results = results
                        displayed_matched_count = matched_count
                        displayed_total_count = total_count
                    else:
                        if queued_search_key != cache_key:
                            # Incremental candidate filtering stays in the daemon;
                            # never serialize cached index arrays through the socket.
                            search_requests.put((cache_key, None, current_cwd_text))
                            queued_search_key = cache_key
                        matched_indices = displayed_matched_indices
                        results = filter_exact_query_match(query, displayed_results)
                        matched_count = displayed_matched_count
                        total_count = displayed_total_count
                    runtime_limit = 1
                    if len(results) == 1:
                        runtime_limit = 2
                    elif not results:
                        runtime_limit = 3
                    runtime_cache_key = (query, cursor_pos)
                    runtime_completions = runtime_completion_cache.get(runtime_cache_key)
                    if runtime_completions is None:
                        runtime_completions = runtime_completion_matches(
                            query,
                            cursor_pos,
                            startup_entries,
                            cwd=current_cwd_path,
                            limit=MAX_RETURNED_RESULTS,
                        )
                        if len(runtime_completion_cache) >= 128:
                            runtime_completion_cache.clear()
                        runtime_completion_cache[runtime_cache_key] = runtime_completions
                    results = insert_runtime_completions(
                        results,
                        runtime_completions,
                        featured_count=runtime_limit,
                    )
                    if preferred_runtime_row is not None:
                        runtime_row = 0
                        if 0 <= runtime_row < len(results) and results[runtime_row].runtime_completion:
                            selected = runtime_row
                        preferred_runtime_row = None
                    last_query = query
                    last_matched_indices = matched_indices
                    status_message = ""
                    debug_note = ""
                    if history_client.debug:
                        count_text = "?" if matched_count is None else str(matched_count)
                        indices_text = "no-idx" if matched_indices is None else "idx"
                        debug_note = f"matches={count_text} {indices_text}"
                    if history_load_error and not results:
                        status_message = "history load failed"
                    if selected >= len(results):
                        selected = max(0, len(results) - 1)
                    if selected < offset:
                        offset = selected
                    if selected >= offset + visible:
                        offset = selected - visible + 1

                    syntax_tokens = syntax_highlighter.highlight(query)
                    query_start, _query_view_len, query_rows_used, results_visible = draw_panel(
                        anchor_row,
                        anchor_col,
                        query,
                        cursor_pos,
                        sel_anchor,
                        sel_end,
                        results,
                        selected,
                        offset,
                        panel_rows,
                        width,
                        clear_previous_cursor=not skip_previous_cursor_clear,
                        status_message=status_message,
                        debug_note=debug_note,
                        total_count=total_count,
                        syntax_tokens=syntax_tokens,
                        query_rows=query_rows,
                        render_line_cache=render_line_cache,
                    )
                    skip_previous_cursor_clear = False
                    last_drawn_panel_rows = panel_rows
                    # Keep the physical cursor at the stable prompt position
                    # while waiting for the next keypress.
                    term_write(move_to(start_row, start_col))
                    term_flush()

                    if pending_event is None:
                        input_timeout: Optional[float] = 0.03
                        if queued_search_key is None:
                            input_timeout = None
                        ev, payload = read_key(fd, timeout=input_timeout)
                    else:
                        ev, payload = pending_event
                    if ev == "timeout":
                        continue

                    prepare_for_keypress()
    
                    if ev == "interrupt":
                        clear_panel_and_restore_cursor(clear_display=True)
                        return None
                    if ev == "escape":
                        clear_panel_and_restore_cursor()
                        return None
                    if ev == "enter":
                        chosen = query
                        break
                    if ev == "tab":
                        if 0 <= selected < len(results):
                            selected_result = results[selected]
                            preferred_runtime_row = 0 if selected_result.runtime_completion else None
                            token_start, token_end = token_bounds(query, cursor_pos)
                            quote, _closes_quote = enclosing_quote(query[token_start:token_end])
                            trailing_text_length = len(query) - token_end
                            query = selected_result.text
                            cursor_pos = len(query)
                            if selected_result.runtime_completion:
                                cursor_pos = max(0, len(query) - trailing_text_length)
                                if quote is not None:
                                    cursor_pos = max(token_start, cursor_pos - 1)
                            clear_selection()
                            sync_mouse_mode()
                            if preferred_runtime_row is None:
                                selected = 0
                            offset = 0
                            clear_after_query_suffix()
                        continue
                    if ev == "left":
                        move_cursor(cursor_pos - 1)
                        continue
                    if ev == "right":
                        move_cursor(cursor_pos + 1)
                        continue
                    if ev == "shift_left":
                        move_cursor(cursor_pos - 1, select_mode=True)
                        continue
                    if ev == "shift_right":
                        move_cursor(cursor_pos + 1, select_mode=True)
                        continue
                    if ev == "home":
                        move_cursor(0)
                        continue
                    if ev == "shift_home":
                        move_cursor(0, select_mode=True)
                        continue
                    if ev == "end":
                        move_cursor(len(query))
                        continue
                    if ev == "shift_end":
                        move_cursor(len(query), select_mode=True)
                        continue
                    if ev == "word_left":
                        move_cursor(move_word_left(query, cursor_pos))
                        continue
                    if ev == "word_right":
                        move_cursor(move_word_right(query, cursor_pos))
                        continue
                    if ev == "select_all":
                        select_all_query()
                        continue
                    if ev == "up":
                        selected = max(0, selected - 1)
                        continue
                    if ev == "down":
                        selected = min(max(0, len(results) - 1), selected + 1)
                        continue
                    if ev == "pgup":
                        selected = max(0, selected - visible)
                        continue
                    if ev == "pgdn":
                        selected = min(max(0, len(results) - 1), selected + visible)
                        continue
                    if ev == "backspace":
                        sel = selection_bounds(sel_anchor, sel_end)
                        if sel:
                            query = query[: sel[0]] + query[sel[1] :]
                            cursor_pos = sel[0]
                            clear_selection()
                        elif cursor_pos > 0:
                            query = query[: cursor_pos - 1] + query[cursor_pos:]
                            cursor_pos -= 1
                        sync_mouse_mode()
                        selected = 0
                        offset = 0
                        continue
                    if ev == "backspace_word":
                        sel = selection_bounds(sel_anchor, sel_end)
                        if sel:
                            query = query[: sel[0]] + query[sel[1] :]
                            cursor_pos = sel[0]
                            clear_selection()
                        else:
                            new_pos = move_word_left(query, cursor_pos)
                            if new_pos < cursor_pos:
                                query = query[:new_pos] + query[cursor_pos:]
                                cursor_pos = new_pos
                        sync_mouse_mode()
                        selected = 0
                        offset = 0
                        continue
                    if ev == "kill_to_start":
                        sel = selection_bounds(sel_anchor, sel_end)
                        if sel:
                            query = query[: sel[0]] + query[sel[1] :]
                            cursor_pos = sel[0]
                        else:
                            query = query[cursor_pos:]
                            cursor_pos = 0
                        clear_selection()
                        sync_mouse_mode()
                        selected = 0
                        offset = 0
                        continue
                    if ev == "kill_to_end":
                        sel = selection_bounds(sel_anchor, sel_end)
                        if sel:
                            query = query[: sel[0]] + query[sel[1] :]
                            cursor_pos = sel[0]
                        else:
                            query = query[:cursor_pos]
                        clear_selection()
                        sync_mouse_mode()
                        selected = 0
                        offset = 0
                        continue
                    if ev == "delete":
                        sel = selection_bounds(sel_anchor, sel_end)
                        if sel:
                            query = query[: sel[0]] + query[sel[1] :]
                            cursor_pos = sel[0]
                            clear_selection()
                        elif cursor_pos < len(query):
                            query = query[:cursor_pos] + query[cursor_pos + 1 :]
                        sync_mouse_mode()
                        selected = 0
                        offset = 0
                        continue
                    if ev == "char":
                        ch = str(payload)
                        if ch == " " and not query and empty_space_command is not None:
                            chosen = empty_space_command
                            skip_history_record = True
                            break
                        sel = selection_bounds(sel_anchor, sel_end)
                        if sel:
                            query = query[: sel[0]] + ch + query[sel[1] :]
                            cursor_pos = sel[0] + 1
                            clear_selection()
                        else:
                            skip_previous_cursor_clear = True
                            query = query[:cursor_pos] + ch + query[cursor_pos:]
                            cursor_pos += 1
                        sync_mouse_mode()
                        selected = 0
                        offset = 0
                        continue
                    if ev == "copy":
                        sel = selection_bounds(sel_anchor, sel_end)
                        if sel:
                            write_clipboard(query[sel[0] : sel[1]])
                        elif query:
                            write_clipboard(query)
                        continue
                    if ev == "paste":
                        pasted = normalize_pasted_text(read_clipboard())
                        if not pasted:
                            continue
                        sel = selection_bounds(sel_anchor, sel_end)
                        if sel:
                            query = query[: sel[0]] + pasted + query[sel[1] :]
                            cursor_pos = sel[0] + len(pasted)
                            clear_selection()
                        else:
                            query = query[:cursor_pos] + pasted + query[cursor_pos:]
                            cursor_pos += len(pasted)
                        sync_mouse_mode()
                        selected = 0
                        offset = 0
                        continue
                    if ev == "paste_text":
                        pasted = normalize_pasted_text(str(payload))
                        if not pasted:
                            continue
                        sel = selection_bounds(sel_anchor, sel_end)
                        if sel:
                            query = query[: sel[0]] + pasted + query[sel[1] :]
                            cursor_pos = sel[0] + len(pasted)
                            clear_selection()
                        else:
                            query = query[:cursor_pos] + pasted + query[cursor_pos:]
                            cursor_pos += len(pasted)
                        sync_mouse_mode()
                        selected = 0
                        offset = 0
                        continue
                    if ev == "mouse":
                        bstate, mx, my, action = payload  # type: ignore[misc]
                        if bstate & 64:
                            # Mouse wheel: mirror up/down arrow behavior.
                            if action != "M":
                                continue
                            wheel_button = bstate & 3
                            if wheel_button == 0:
                                selected = max(0, selected - 1)
                            elif wheel_button == 1:
                                selected = min(max(0, len(results) - 1), selected + 1)
                            continue
                        button = bstate & 3
                        is_motion = bool(bstate & 32)
                        is_shift = bool(bstate & 4)
    
                        # SGR mouse uses 'M' for press/motion, 'm' for release.
                        if action == "m":
                            if button in (0, 3):
                                mouse_selecting = False
                            continue
                        if action != "M":
                            continue
    
                        # Query line interactions (including wrapped rows).
                        if anchor_row <= my < (anchor_row + query_rows_used):
                            click_row = my - anchor_row
                            absolute_query_row = query_start + click_row
                            click_col = query_click_visual_col(
                                mx,
                                absolute_query_row,
                                anchor_col,
                            )
                            click_pos = query_pos_from_visual(
                                query,
                                query_width,
                                query_start,
                                click_row,
                                click_col,
                                continuation_query_width,
                            )
    
                            if is_motion:
                                if mouse_selecting:
                                    move_cursor(click_pos, select_mode=True)
                                continue
    
                            if button == 0:
                                now = time.monotonic()
                                is_same_click_area = (
                                    (now - last_left_click_time) <= 0.35
                                    and my == last_left_click_row
                                    and abs(mx - last_left_click_col) <= 1
                                )
                                if is_same_click_area:
                                    left_click_count += 1
                                else:
                                    left_click_count = 1
                                last_left_click_time = now
                                last_left_click_row = my
                                last_left_click_col = mx
    
                                if left_click_count >= 3 and query:
                                    select_all_query()
                                    mouse_selecting = False
                                elif left_click_count == 2 and query:
                                    # Select the contiguous run under the cursor:
                                    # either non-whitespace ("word") or whitespace.
                                    left = click_pos
                                    right = click_pos
                                    if click_pos < len(query):
                                        select_whitespace = query[click_pos].isspace()
                                        while left > 0 and query[left - 1].isspace() == select_whitespace:
                                            left -= 1
                                        right = click_pos + 1
                                        while right < len(query) and query[right].isspace() == select_whitespace:
                                            right += 1
                                    else:
                                        while left > 0 and not query[left - 1].isspace():
                                            left -= 1
                                    if left != right:
                                        sel_anchor = left
                                        sel_end = right
                                        cursor_pos = right
                                    else:
                                        move_cursor(click_pos, select_mode=is_shift)
                                else:
                                    move_cursor(click_pos, select_mode=is_shift)
                                    mouse_selecting = True
                                continue
    
                        # Ignore result-line clicks; selection/accept remains
                        # keyboard-driven (arrows + Enter/Tab).
                        if my >= (anchor_row + query_rows_used) and my < anchor_row + panel_rows and not is_motion and button == 0:
                            continue
            except KeyboardInterrupt:
                clear_panel_and_restore_cursor(clear_display=True)
                return None
            finally:
                search_stop.set()
                search_requests.put(None)

            clear_panel_and_restore_cursor()
            if chosen is None:
                return None
            return chosen, skip_history_record
    finally:
        engine.TERM_OUT = sys.stdout
        if tty_in_file is not None:
            tty_in_file.close()
        if tty_out_file is not None:
            tty_out_file.close()


def main() -> int:
    parser = ArgumentParser(add_help=True)
    parser.add_argument(
        "--print-only",
        action="store_true",
        help="Print selected command to stdout instead of executing it.",
    )
    parser.add_argument(
        "--no-save-history",
        action="store_true",
        help="Do not add the selected command to custom history.",
    )
    parser.add_argument("--daemon", action="store_true", help=SUPPRESS)
    parser.add_argument("--socket-path", default="", help=SUPPRESS)
    parser.add_argument("--history-file", default="", help=SUPPRESS)
    parser.add_argument(
        "--history-length",
        default=None,
        help="Maximum SQLite history rows to load on initial custom-history startup (for example: 10000 or 10k).",
    )
    parser.add_argument(
        "--debug-daemon",
        action="store_true",
        help="Print daemon connection/startup diagnostics to stderr.",
    )
    parser.add_argument(
        "--use-custom-history",
        action="store_true",
        help="Use per-user SQLite history (command, cwd, timestamp).",
    )
    parser.add_argument(
        "--record-status",
        action="store_true",
        help=SUPPRESS,
    )
    parser.add_argument(
        "--status-command",
        default="",
        help=SUPPRESS,
    )
    parser.add_argument(
        "--status-code",
        type=int,
        default=0,
        help=SUPPRESS,
    )
    parser.add_argument(
        "--status-cwd",
        default="",
        help=SUPPRESS,
    )
    args = parser.parse_args()
    history_length: Optional[int] = None
    if args.history_length is not None:
        try:
            history_length = parse_history_length_arg(str(args.history_length))
        except ValueError as exc:
            print(f"zsh_flex_history: {exc}", file=sys.stderr)
            return 2

    if args.use_custom_history:
        history_path = default_custom_history_path()
        try:
            ensure_custom_history_file(history_path)
        except OSError as exc:
            print(f"zsh_flex_history: failed to initialize custom history file: {exc}", file=sys.stderr)
            return 1
    else:
        history_path_value = args.history_file or os.environ.get("HISTFILE", str(Path.home() / ".zsh_history"))
        history_path = Path(history_path_value).expanduser()

    if args.record_status:
        if not args.use_custom_history:
            print("zsh_flex_history: --record-status requires --use-custom-history", file=sys.stderr)
            return 2
        return 0 if update_custom_history_exit_status(
            history_path,
            args.status_command,
            args.status_cwd or os.getcwd(),
            args.status_code,
        ) else 1

    socket_path = (
        Path(args.socket_path).expanduser()
        if args.socket_path
        else default_daemon_socket_path(use_custom_history=args.use_custom_history)
    )

    if args.daemon:
        return run_history_daemon(
            history_path,
            socket_path,
            debug=args.debug_daemon,
            history_length=history_length,
            use_custom_history=args.use_custom_history,
        )

    history_client = HistoryDaemonClient(
        socket_path,
        history_path,
        Path(__file__).resolve(),
        debug=args.debug_daemon,
        history_length=history_length,
        use_custom_history=args.use_custom_history,
    )
    if not history_client.ensure_running():
        print("zsh_flex_history: daemon unavailable", file=sys.stderr)
        return 1
    daemon_debug_log(args.debug_daemon, "shared daemon mode enabled")

    empty_space_command = os.environ.get("ZSH_FLEX_HISTORY_EMPTY_SPACE_COMMAND")
    if empty_space_command is not None and not empty_space_command.strip():
        empty_space_command = None
    selection = run(
        inline_with_prompt=args.print_only,
        history_client=history_client,
        empty_space_command=empty_space_command,
    )
    if selection is not None:
        selected, skip_history_record = selection
        selected = selected.replace("\r\n", "\n").replace("\r", "\n").replace("\x00", "")
        if not selected.strip():
            return 1
        if args.use_custom_history and not args.no_save_history and not skip_history_record:
            append_custom_history_entry(
                history_path,
                selected,
                os.getcwd(),
                datetime.now(timezone.utc).isoformat(),
            )
        if args.print_only:
            print(selected)
            return 0
        shell = os.environ.get("SHELL", "/bin/zsh")
        print(f"$ {selected}")
        completed = subprocess.run([shell, "-lc", selected])
        return completed.returncode
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
