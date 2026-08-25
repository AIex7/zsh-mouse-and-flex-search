
from __future__ import annotations

import os
import re
import select
import shlex
import shutil
import sqlite3
import subprocess
import sys
import termios
import threading
import time
import tempfile
import tty
from datetime import datetime, timezone
from dataclasses import dataclass, replace
from pathlib import Path
from typing import Any, Optional, Sequence

from ._flex_match import NativeDaemonServer as _NativeDaemonServer
from ._flex_match import NativeHistory as _NativeHistory
from ._flex_match import flex_match as _native_flex_match
from ._flex_match import ping_daemon as _native_ping_daemon
from ._flex_match import search_daemon as _native_search_daemon

ANSI_COLOR_NAMES = {
    "black": 0,
    "red": 1,
    "green": 2,
    "yellow": 3,
    "blue": 4,
    "magenta": 5,
    "purple": 5,
    "cyan": 6,
    "white": 7,
    "bright-black": 8,
    "gray": 8,
    "grey": 8,
    "bright-red": 9,
    "bright-green": 10,
    "bright-yellow": 11,
    "bright-blue": 12,
    "bright-magenta": 13,
    "bright-purple": 13,
    "bright-cyan": 14,
    "bright-white": 15,
}

DORIC = {
    "cursor": "#205798",
    "bg_main": "#fcf0e5",
    "fg_main": "#40282e",
    "border": "#c3a8bf",
    "bg_shadow_subtle": "#efe4db",
    "fg_shadow_subtle": "#8f5854",
    "bg_neutral": "#e6d5d0",
    "fg_neutral": "#514250",
    "bg_shadow_intense": "#fcb894",
    "fg_shadow_intense": "#a02016",
    "bg_accent": "#c8f0e3",
    "fg_accent": "#085078",
    "fg_red": "#a02610",
    "fg_green": "#006940",
    "fg_yellow": "#753800",
    "fg_blue": "#183182",
    "fg_magenta": "#820145",
    "fg_cyan": "#025763",
    "bg_red": "#ffbca7",
    "bg_green": "#b2efd8",
    "bg_yellow": "#e6e294",
    "bg_blue": "#baceef",
    "bg_magenta": "#e2c1e0",
    "bg_cyan": "#c0e6f9",
}

@dataclass
class MatchResult:
    text: str
    score: int
    exact: bool = False
    recency: int = 0
    cwd: Optional[str] = None
    text_lower: Optional[str] = None
    runtime_completion: bool = False
    runtime_completion_span: Optional[tuple[int, int]] = None
    failed: bool = False
    words: tuple[str, ...] = ()


@dataclass
class HistoryEntry:
    text: str
    cwd: Optional[str] = None
    text_lower: str = ""
    timestamp: Optional[str] = None
    failed: bool = False
    words: tuple[str, ...] = ()


@dataclass(frozen=True)
class DirectoryListingEntry:
    name: str
    is_dir: bool


_DIRECTORY_LISTING_CACHE: dict[Path, tuple[DirectoryListingEntry, ...]] = {}
_DIRECTORY_LISTING_CACHE_ORDER: list[Path] = []
_DIRECTORY_LISTING_CACHE_LIMIT = 128
_DIRECTORY_LISTING_CACHE_LOCK = threading.Lock()


def cached_directory_listing(directory: Path) -> Optional[tuple[DirectoryListingEntry, ...]]:
    try:
        cache_key = directory.resolve()
    except OSError:
        return None

    with _DIRECTORY_LISTING_CACHE_LOCK:
        cached = _DIRECTORY_LISTING_CACHE.get(cache_key)
        if cached is not None:
            return cached

    try:
        entries: list[DirectoryListingEntry] = []
        with os.scandir(cache_key) as scanned_entries:
            for entry in scanned_entries:
                try:
                    # Follow symlinks so paths such as macOS's /etc symlink
                    # are still completed as directories with a trailing '/'.
                    is_dir = entry.is_dir()
                except OSError:
                    continue
                entries.append(DirectoryListingEntry(entry.name, is_dir))
    except OSError:
        return None

    cached_entries = tuple(entries)
    with _DIRECTORY_LISTING_CACHE_LOCK:
        existing = _DIRECTORY_LISTING_CACHE.get(cache_key)
        if existing is not None:
            return existing
        if len(_DIRECTORY_LISTING_CACHE_ORDER) >= _DIRECTORY_LISTING_CACHE_LIMIT:
            oldest = _DIRECTORY_LISTING_CACHE_ORDER.pop(0)
            _DIRECTORY_LISTING_CACHE.pop(oldest, None)
        _DIRECTORY_LISTING_CACHE_ORDER.append(cache_key)
        _DIRECTORY_LISTING_CACHE[cache_key] = cached_entries
    return cached_entries


def fg_code(slot: int) -> str:
    if 0 <= slot <= 7:
        return str(30 + slot)
    if 8 <= slot <= 15:
        return str(90 + (slot - 8))
    return "39"


def bg_code(slot: int) -> str:
    if 0 <= slot <= 7:
        return str(40 + slot)
    if 8 <= slot <= 15:
        return str(100 + (slot - 8))
    return "49"


def ansi_color_from_env(name: str, default: Optional[int]) -> Optional[int]:
    raw = os.environ.get(name, "").strip().lower().replace("_", "-")
    if not raw:
        return default
    if raw.isdigit():
        value = int(raw)
        if 0 <= value <= 15:
            return value
        return default
    return ANSI_COLOR_NAMES.get(raw, default)


def color_value_from_env(name: str) -> tuple[Optional[int], Optional[str]]:
    raw = os.environ.get(name, "").strip().lower()
    if raw in ("none", "disabled", "unsupported", "off", "0"):
        return None, None
    if raw.startswith("#") and re.fullmatch(r"#[0-9a-f]{6}", raw):
        return None, raw
    return ansi_color_from_env(name, None), None


def positive_int_from_env(name: str, default: int) -> int:
    raw = os.environ.get(name, "").strip()
    try:
        value = int(raw)
    except ValueError:
        return default
    return value if value > 0 else default


def hex_to_rgb(value: str) -> tuple[int, int, int]:
    value = value.lstrip("#")
    return int(value[0:2], 16), int(value[2:4], 16), int(value[4:6], 16)


def rgb_to_hex(r: int, g: int, b: int) -> str:
    return f"#{max(0, min(r, 255)):02x}{max(0, min(g, 255)):02x}{max(0, min(b, 255)):02x}"


def style(
    *,
    fg: Optional[int] = None,
    bg: Optional[int] = None,
    fg_rgb: Optional[str] = None,
    bg_rgb: Optional[str] = None,
    bold: bool = False,
    dim: bool = False,
    italic: bool = False,
    underline: bool = False,
) -> str:
    codes: list[str] = []
    if bold:
        codes.append("1")
    if dim:
        codes.append("2")
    if italic:
        codes.append("3")
    if underline:
        codes.append("4")
    if fg is not None:
        codes.append(fg_code(fg))
    if bg is not None:
        codes.append(bg_code(bg))
    if fg_rgb is not None:
        r, g, b = hex_to_rgb(fg_rgb)
        codes.extend(["38", "2", str(r), str(g), str(b)])
    if bg_rgb is not None:
        r, g, b = hex_to_rgb(bg_rgb)
        codes.extend(["48", "2", str(r), str(g), str(b)])
    if not codes:
        return ""
    return f"\x1b[{';'.join(codes)}m"


RESET = "\x1b[0m"
QUERY_SELECTION_BG = style(fg_rgb=DORIC["fg_blue"], bg_rgb=DORIC["bg_blue"])
_cursor_color_env_raw = os.environ.get("ZSH_FLEX_HISTORY_CURSOR_COLOR")
_cursor_color_configured = _cursor_color_env_raw is not None and bool(_cursor_color_env_raw.strip())
_cursor_color, _cursor_color_rgb = color_value_from_env("ZSH_FLEX_HISTORY_CURSOR_COLOR")
VISUAL_CURSOR_BG = (
    style(fg_rgb=DORIC["fg_main"], bg=_cursor_color)
    if _cursor_color is not None
    else style(fg_rgb=DORIC["fg_main"], bg_rgb=_cursor_color_rgb)
    if _cursor_color_rgb is not None
    else style(fg_rgb=DORIC["fg_main"], bg_rgb=DORIC["bg_accent"])
)
CLEAR_TO_END = "\x1b[K"
HIDE_CURSOR = "\x1b[?25l"
SHOW_CURSOR = "\x1b[?25h"
ENABLE_MOUSE = "\x1b[?1000h\x1b[?1002h\x1b[?1006h"
DISABLE_MOUSE = "\x1b[?1000l\x1b[?1002l\x1b[?1006l"
ENABLE_KITTY_KEYBOARD = "\x1b[>1u"
DISABLE_KITTY_KEYBOARD = "\x1b[<u"
MAX_RETURNED_RESULTS_ENV = "ZSH_FLEX_HISTORY_MAX_RETURNED_RESULTS"
DEFAULT_MAX_RETURNED_RESULTS = 100
MAX_RETURNED_RESULTS = positive_int_from_env(
    MAX_RETURNED_RESULTS_ENV,
    DEFAULT_MAX_RETURNED_RESULTS,
)
MAX_CACHED_CANDIDATE_INDICES = 10_000
FIXED_MATCH_TEXT_WIDTH = 3000
RESULT_PREFIX_WIDTH = 2
SELECTOR_GLYPH = "●"
FAILED_SELECTOR_GLYPH = "○"
SELECTOR_GLYPH_ENV = "ZSH_FLEX_HISTORY_SELECTOR_GLYPH"
FAILED_SELECTOR_GLYPH_ENV = "ZSH_FLEX_HISTORY_FAILED_SELECTOR_GLYPH"


def glyph_from_env(name: str, default: str) -> str:
    value = os.environ.get(name, "").strip()
    return value[:1] or default

TERM_OUT = sys.stdout


def move_to(row: int, col: int = 1) -> str:
    return f"\x1b[{max(1, row)};{max(1, col)}H"


def term_write(text: str) -> None:
    TERM_OUT.write(text)


def term_flush() -> None:
    TERM_OUT.flush()


def tty_terminal_size(fd: int, fallback: tuple[int, int] = (120, 24)) -> os.terminal_size:
    try:
        return os.get_terminal_size(fd)
    except OSError:
        size = shutil.get_terminal_size(fallback)
        return os.terminal_size((max(1, size.columns), max(1, size.lines)))


def write_clipboard(text: str) -> bool:
    if shutil.which("pbcopy") is None:
        return False
    try:
        subprocess.run(["pbcopy"], input=text, text=True, check=True)
    except (OSError, subprocess.SubprocessError):
        return False
    return True


def read_clipboard() -> str:
    if shutil.which("pbpaste") is None:
        return ""
    try:
        proc = subprocess.run(["pbpaste"], check=True, capture_output=True, text=True)
    except (OSError, subprocess.SubprocessError):
        return ""
    return proc.stdout.replace("\r\n", "\n").replace("\r", "\n")


def normalize_pasted_text(text: str) -> str:
    # Keep multiline content, but strip terminal control artifacts.
    normalized = text.replace("\r\n", "\n").replace("\r", "\n").replace("\x00", "")
    # Drop CSI sequences (including stray bracketed-paste/mouse reports).
    normalized = re.sub(r"\x1b\[[0-9;?<>]*[ -/]*[@-~]", "", normalized)
    # Drop leaked bracketed-paste markers even if ESC got stripped.
    normalized = normalized.replace("200~", "").replace("201~", "")
    # Drop leaked SGR mouse payloads when ESC is missing.
    normalized = re.sub(r"<\d+;\d+;\d+[mM]", "", normalized)
    return normalized


def supports_kitty_keyboard_protocol() -> bool:
    term = os.environ.get("TERM", "")
    return bool(os.environ.get("KITTY_WINDOW_ID")) or "kitty" in term.lower()


class RawTerminal:
    def __init__(self, fd: int) -> None:
        self.fd = fd
        self._old: Optional[list] = None

    def __enter__(self) -> "RawTerminal":
        self._old = termios.tcgetattr(self.fd)
        tty.setraw(self.fd)
        try:
            termios.tcflush(self.fd, termios.TCIFLUSH)
        except termios.error:
            pass
        # Start with mouse reporting disabled; it will be enabled lazily
        # once the user types the first character in the query.
        term_write(DISABLE_MOUSE)
        term_write(HIDE_CURSOR)
        term_flush()
        return self

    def __exit__(self, exc_type, exc, tb) -> None:
        term_write(DISABLE_MOUSE + SHOW_CURSOR + RESET)
        term_flush()
        if self._old is not None:
            termios.tcsetattr(self.fd, termios.TCSADRAIN, self._old)


def query_cursor_position(fd: int) -> Optional[tuple[int, int]]:
    # Drain any stale input bytes so we do not parse an old cursor response.
    while True:
        ready, _, _ = select.select([fd], [], [], 0)
        if not ready:
            break
        try:
            os.read(fd, 4096)
        except OSError:
            break

    term_write("\x1b[6n")
    term_flush()
    buf = b""
    deadline = time.monotonic() + 0.2
    last_match: Optional[tuple[int, int]] = None
    while time.monotonic() < deadline:
        ready, _, _ = select.select([fd], [], [], 0.02)
        if not ready:
            continue
        buf += os.read(fd, 64)
        for m in re.finditer(rb"\x1b\[(\d+);(\d+)R", buf):
            last_match = (int(m.group(1)), int(m.group(2)))
        if last_match is not None:
            # Return as soon as we have a valid cursor report instead of
            # waiting out the full timeout on every startup.
            break
    return last_match


def _scale_hex_component(component: str) -> int:
    if not component:
        raise ValueError("empty color component")
    value = int(component, 16)
    max_value = (16 ** len(component)) - 1
    if max_value <= 0:
        return 0
    return round((value / max_value) * 255)


def query_cursor_color(fd: int) -> Optional[str]:
    """Return the terminal's cursor color from OSC 12, when supported."""
    env_cached = os.environ.get("ZSH_FLEX_HISTORY_CURSOR_COLOR")
    if env_cached is not None:
        val = env_cached.strip().lower()
        if val in ("", "none", "disabled", "unsupported", "off", "0"):
            return None
        if val.startswith("#") and re.fullmatch(r"#[0-9a-f]{6}", val):
            return val

    while True:
        ready, _, _ = select.select([fd], [], [], 0)
        if not ready:
            break
        try:
            os.read(fd, 4096)
        except OSError:
            break

    term_write("\x1b]12;?\x07")
    term_flush()
    buf = bytearray()
    deadline = time.monotonic() + 0.010
    while time.monotonic() < deadline:
        ready, _, _ = select.select([fd], [], [], 0.002)
        if not ready:
            continue
        try:
            chunk = os.read(fd, 128)
        except OSError:
            os.environ["ZSH_FLEX_HISTORY_CURSOR_COLOR"] = "none"
            return None
        if not chunk:
            continue
        buf.extend(chunk)
        if b"\x07" in buf or b"\x1b\\" in buf:
            break

    match = re.search(rb"\x1b\]12;rgb:([0-9a-fA-F]+)/([0-9a-fA-F]+)/([0-9a-fA-F]+)(?:\x07|\x1b\\)", bytes(buf))
    if match is None:
        os.environ["ZSH_FLEX_HISTORY_CURSOR_COLOR"] = "none"
        return None
    try:
        r = _scale_hex_component(match.group(1).decode("ascii"))
        g = _scale_hex_component(match.group(2).decode("ascii"))
        b = _scale_hex_component(match.group(3).decode("ascii"))
    except (UnicodeDecodeError, ValueError):
        os.environ["ZSH_FLEX_HISTORY_CURSOR_COLOR"] = "none"
        return None
    hex_color = rgb_to_hex(r, g, b)
    os.environ["ZSH_FLEX_HISTORY_CURSOR_COLOR"] = hex_color
    return hex_color


def normalize_cwd_value(cwd: str) -> str:
    stripped = cwd.strip()
    if not stripped:
        return ""
    return os.path.normpath(stripped)


def make_history_entry(
    text: str,
    *,
    cwd: Optional[str] = None,
    timestamp: Optional[str] = None,
    failed: bool = False,
) -> HistoryEntry:
    return HistoryEntry(
        text=text,
        cwd=cwd,
        text_lower=text.lower(),
        timestamp=timestamp,
        failed=failed,
        words=tuple(shell_words_for_matching(text)),
    )


def load_history(path: Path) -> list[HistoryEntry]:
    entries: list[HistoryEntry] = []
    if not path.exists():
        return entries

    raw = path.read_text(encoding="utf-8", errors="replace")
    normalized = raw.replace("\r\n", "\n").replace("\r", "\n")

    # Support plain history and extended history in the same file.
    # Extended entry format:
    #   : 1700012345:0;command
    header_line_re = re.compile(r"^: \d+:\d+;(.*)$")
    current_extended: Optional[str] = None

    def push_entry(text: str) -> None:
        cmd = text.rstrip("\n").replace("\\\n", "").strip()
        if cmd:
            entries.append(make_history_entry(cmd))

    for line in normalized.split("\n"):
        match = header_line_re.match(line)
        if match:
            if current_extended is not None:
                push_entry(current_extended)
            current_extended = match.group(1)
            continue

        if current_extended is not None:
            current_extended += "\n" + line
            continue

        plain = line.strip()
        if plain:
            entries.append(make_history_entry(plain))

    if current_extended is not None:
        push_entry(current_extended)

    # Preserve recency ordering (newest first), then remove duplicate command
    # text while keeping the newest occurrence of each command.
    newest_first = list(reversed(entries))
    return dedupe_history_entries_preserving_order(newest_first)


def dedupe_history_entries_preserving_order(entries: list[HistoryEntry]) -> list[HistoryEntry]:
    deduped: list[HistoryEntry] = []
    seen: set[str] = set()
    for entry in entries:
        if entry.text in seen:
            continue
        seen.add(entry.text)
        deduped.append(entry)
    return deduped


def default_app_state_dir() -> Path:
    xdg_state_home = os.environ.get("XDG_STATE_HOME", "").strip()
    if xdg_state_home:
        return Path(xdg_state_home).expanduser() / "zsh-flex-history"
    if sys.platform == "darwin":
        return Path.home() / "Library" / "Application Support" / "zsh-flex-history"
    return Path.home() / ".local" / "state" / "zsh-flex-history"


def default_custom_history_path() -> Path:
    return default_app_state_dir() / "history.db"


def default_history_log_path() -> Path:
    env_log = os.environ.get("ZSH_FLEX_HISTORY_LOG_FILE")
    if env_log:
        return Path(env_log).expanduser()
    return default_app_state_dir() / "history_rebuild.log"


def log_database_load_event(event_type: str, details: str = "", *, log_path: Optional[Path] = None) -> None:
    target_path = log_path or default_history_log_path()
    try:
        target_path.parent.mkdir(parents=True, exist_ok=True)
        now_str = datetime.now().astimezone().isoformat()
        msg = f"[{now_str}] {event_type}"
        if details:
            msg += f" - {details}"
        with open(target_path, "a", encoding="utf-8") as f:
            f.write(msg + "\n")
    except OSError:
        pass


def parse_history_length_arg(raw: str) -> int:
    value = raw.strip().lower().replace("_", "")
    match = re.fullmatch(r"(\d+)([km]?)", value)
    if match is None:
        raise ValueError(f"invalid history length: {raw!r}")
    count = int(match.group(1))
    suffix = match.group(2)
    if suffix == "k":
        count *= 1_000
    elif suffix == "m":
        count *= 1_000_000
    if count <= 0:
        raise ValueError("history length must be positive")
    return count


def ensure_custom_history_file(path: Path) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with sqlite3.connect(path) as conn:
        conn.execute(
            """
            CREATE TABLE IF NOT EXISTS custom_history (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                command TEXT NOT NULL,
                cwd TEXT NOT NULL,
                timestamp TEXT NOT NULL,
                failed INTEGER NOT NULL DEFAULT 0,
                status_revision INTEGER NOT NULL DEFAULT 0
            )
            """
        )
        columns = {row[1] for row in conn.execute("PRAGMA table_info(custom_history)").fetchall()}
        if "failed" not in columns:
            conn.execute("ALTER TABLE custom_history ADD COLUMN failed INTEGER NOT NULL DEFAULT 0")
        if "status_revision" not in columns:
            conn.execute(
                "ALTER TABLE custom_history ADD COLUMN status_revision INTEGER NOT NULL DEFAULT 0"
            )
        conn.execute(
            """
            CREATE TABLE IF NOT EXISTS custom_history_metadata (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                status_revision INTEGER NOT NULL DEFAULT 0
            )
            """
        )
        conn.execute(
            "INSERT OR IGNORE INTO custom_history_metadata(id, status_revision) VALUES(1, 0)"
        )
        conn.execute(
            """
            UPDATE custom_history_metadata
            SET status_revision = MAX(
                status_revision,
                COALESCE((SELECT MAX(status_revision) FROM custom_history), 0)
            )
            WHERE id = 1
            """
        )
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_custom_history_command_cwd ON custom_history(command, cwd)"
        )
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_custom_history_id_desc ON custom_history(id DESC)"
        )
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_custom_history_status_revision "
            "ON custom_history(status_revision)"
        )
        conn.commit()


def custom_history_entry_from_row(row: object) -> Optional[HistoryEntry]:
    if not isinstance(row, tuple) or len(row) < 3:
        return None
    cmd = row[0]
    cwd = row[1]
    timestamp = row[2]
    failed = row[3] if len(row) >= 4 else 0
    if not isinstance(cmd, str):
        return None
    normalized_cwd = normalize_cwd_value(cwd) if isinstance(cwd, str) else ""
    normalized_timestamp = timestamp if isinstance(timestamp, str) else None
    cleaned = cmd.replace("\r\n", "\n").replace("\r", "\n").replace("\x00", "").strip("\n")
    if not cleaned.strip():
        return None
    return make_history_entry(
        cleaned,
        cwd=normalized_cwd or None,
        timestamp=normalized_timestamp,
        failed=bool(failed),
    )


def custom_history_entries_from_rows(rows: Sequence[object]) -> list[HistoryEntry]:
    return [entry for row in rows if (entry := custom_history_entry_from_row(row)) is not None]


def load_custom_history_rows(path: Path, *, limit: Optional[int] = None) -> list[HistoryEntry]:
    if not path.exists():
        return []
    query = "SELECT command, cwd, timestamp, failed FROM custom_history ORDER BY id DESC"
    params: tuple[object, ...] = ()
    if limit is not None and limit > 0:
        query += " LIMIT ?"
        params = (limit,)
    try:
        with sqlite3.connect(path) as conn:
            rows = conn.execute(query, params).fetchall()
    except (OSError, sqlite3.Error):
        return []
    return custom_history_entries_from_rows(rows)


def load_custom_history(
    path: Path,
    *,
    limit: Optional[int] = None,
) -> list[HistoryEntry]:
    return load_custom_history_rows(path, limit=limit)


def load_history_source(
    path: Path,
    *,
    use_custom_history: bool,
    history_length: Optional[int] = None,
) -> list[HistoryEntry]:
    if use_custom_history:
        return load_custom_history(path, limit=history_length)
    return load_history(path)


def append_custom_history_entry(path: Path, command: str, cwd: str, timestamp: str) -> bool:
    normalized_command = command.strip()
    normalized_cwd = normalize_cwd_value(cwd)
    if not normalized_command:
        return False
    try:
        if not path.exists():
            ensure_custom_history_file(path)
        with sqlite3.connect(path) as conn:
            conn.execute(
                "DELETE FROM custom_history WHERE command = ? AND cwd = ?",
                (normalized_command, normalized_cwd),
            )
            conn.execute(
                "INSERT INTO custom_history(command, cwd, timestamp, failed) VALUES(?, ?, ?, 0)",
                (normalized_command, normalized_cwd, timestamp),
            )
            conn.commit()
    except (OSError, sqlite3.Error):
        return False
    return True


def parse_iso_datetime(value: str) -> Optional[datetime]:
    try:
        parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        return None
    if parsed.tzinfo is None:
        return parsed.replace(tzinfo=timezone.utc)
    return parsed.astimezone(timezone.utc)


def update_custom_history_exit_status(
    path: Path,
    command: str,
    cwd: str,
    status: int,
    *,
    max_age_seconds: int = 24 * 60 * 60,
) -> bool:
    normalized_command = command.strip()
    normalized_cwd = normalize_cwd_value(cwd)
    if not normalized_command:
        return False

    try:
        if not path.exists():
            return False
        with sqlite3.connect(path) as conn:
            # Select and update the same command row while excluding concurrent writers.
            conn.execute("BEGIN IMMEDIATE")
            row = conn.execute(
                """
                SELECT id, timestamp
                FROM custom_history
                WHERE command = ? AND cwd = ?
                ORDER BY id DESC
                LIMIT 1
                """,
                (normalized_command, normalized_cwd),
            ).fetchone()
            if not isinstance(row, tuple) or len(row) < 2:
                return False
            row_id, timestamp = row
            if not isinstance(row_id, int) or not isinstance(timestamp, str):
                return False
            parsed_timestamp = parse_iso_datetime(timestamp)
            if parsed_timestamp is None:
                return False
            age = datetime.now(timezone.utc) - parsed_timestamp
            if age.total_seconds() < 0 or age.total_seconds() > max_age_seconds:
                return False
            if status == 0:
                conn.commit()
                return True
            conn.execute(
                "UPDATE custom_history_metadata SET status_revision = status_revision + 1 WHERE id = 1"
            )
            revision_row = conn.execute(
                "SELECT status_revision FROM custom_history_metadata WHERE id = 1"
            ).fetchone()
            if not revision_row or not isinstance(revision_row[0], int):
                return False
            conn.execute(
                "UPDATE custom_history SET failed = 1, status_revision = ? WHERE id = ?",
                (revision_row[0], row_id),
            )
            conn.commit()
    except (OSError, sqlite3.Error):
        return False
    return True


def default_daemon_socket_path(*, use_custom_history: bool = False) -> Path:
    runtime_dir = os.environ.get("XDG_RUNTIME_DIR")
    if runtime_dir:
        base_dir = Path(runtime_dir)
    else:
        base_dir = Path(tempfile.gettempdir())
    suffix = "-custom" if use_custom_history else ""
    return base_dir / f"zsh-flex-history-{os.getuid()}{suffix}.sock"


def history_file_signature(path: Path) -> tuple[int, int]:
    try:
        st = path.stat()
    except OSError:
        return (0, 0)
    return (st.st_mtime_ns, st.st_size)


def daemon_debug_log(enabled: bool, message: str) -> None:
    if enabled:
        print(f"[zsh_flex_history daemon] {message}", file=sys.stderr)


def query_equals_candidate(query: str, candidate: str) -> bool:
    normalized_query = query.strip().lower()
    return bool(normalized_query) and candidate.strip().lower() == normalized_query


def filter_exact_query_match(query: str, results: list[MatchResult]) -> list[MatchResult]:
    if not query.strip():
        return results
    return [item for item in results if not query_equals_candidate(query, item.text)]


def flex_match(query: str, candidate: str, *, candidate_lower: Optional[str] = None) -> Optional[MatchResult]:
    """Match with the required native extension."""
    c = candidate_lower or candidate.lower()
    matched = _native_flex_match(query.lower(), candidate, c)
    if matched is None:
        return None
    return MatchResult(candidate, matched, text_lower=c)


def token_bounds(query: str, cursor_pos: int) -> tuple[int, int]:
    cursor = max(0, min(cursor_pos, len(query)))
    tokens: list[tuple[int, int]] = []
    i = 0
    while i < len(query):
        while i < len(query) and query[i].isspace():
            i += 1
        if i >= len(query):
            break
        start = i
        quote: Optional[str] = None
        escaped = False
        while i < len(query):
            ch = query[i]
            if escaped:
                escaped = False
                i += 1
                continue
            if ch == "\\":
                escaped = True
                i += 1
                continue
            if quote is not None:
                if ch == quote:
                    quote = None
                i += 1
                continue
            if ch in ("'", '"'):
                quote = ch
                i += 1
                continue
            if ch.isspace():
                break
            i += 1
        end = i
        tokens.append((start, end))
    for start, end in tokens:
        if start <= cursor <= end:
            return start, end
    return cursor, cursor


def strip_enclosing_quotes(token: str) -> str:
    if len(token) >= 2 and token[0] == token[-1] and token[0] in ("'", '"'):
        return token[1:-1]
    if token.startswith(("'", '"')):
        return token[1:]
    if token.endswith(("'", '"')):
        return token[:-1]
    return token


def enclosing_quote(token: str) -> tuple[Optional[str], bool]:
    if not token:
        return None, False
    if token[0] not in ("'", '"'):
        return None, False
    quote = token[0]
    return quote, len(token) > 1 and token[-1] == quote


def shell_unescape_fragment(text: str) -> str:
    out: list[str] = []
    i = 0
    while i < len(text):
        ch = text[i]
        if ch == "\\" and i + 1 < len(text):
            i += 1
            out.append(text[i])
        else:
            out.append(ch)
        i += 1
    return "".join(out)


def shell_escape_fragment(text: str) -> str:
    escaped: list[str] = []
    safe = set("abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-._/~")
    for ch in text:
        if ch in safe:
            escaped.append(ch)
        else:
            escaped.append("\\" + ch)
    return "".join(escaped)


def shell_escape_quoted_fragment(text: str, quote: str) -> str:
    """Escape text for insertion inside an existing shell-quoted token."""
    if quote == "'":
        # A literal single quote closes the current string, emits an escaped
        # quote, then opens a new single-quoted string.
        return text.replace("'", "'\\\\''")
    # Within double quotes, spaces are literal. Only these characters retain
    # special meaning to the shell and need escaping.
    return "".join("\\\\" + ch if ch in ('\\\\', '"', '$', '`') else ch for ch in text)


def replace_query_token(query: str, cursor_pos: int, replacement: str) -> str:
    start, end = token_bounds(query, cursor_pos)
    return query[:start] + replacement + query[end:]


def top_ranked_directory_entries(
    query: str,
    entries: tuple[DirectoryListingEntry, ...],
) -> list[DirectoryListingEntry]:
    entry_by_name: dict[str, DirectoryListingEntry] = {}
    ranked_candidates: list[MatchResult] = []
    for entry in entries:
        matched = flex_match(query, entry.name)
        if matched is None:
            continue
        entry_by_name[entry.name] = entry
        ranked_candidates.append(matched)

    ranked_results = apply_prefix_priority(query, ranked_candidates)
    ranked_results.sort(key=lambda item: item.score, reverse=True)
    ordered_entries: list[DirectoryListingEntry] = []
    for item in ranked_results:
        entry = entry_by_name.get(item.text)
        if entry is not None:
            ordered_entries.append(entry)
    return ordered_entries


PATH_COMPLETION_ENV_VARS = frozenset(
    {
        "HOME",
        "PWD",
        "OLDPWD",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_CACHE_HOME",
        "XDG_STATE_HOME",
        "TMPDIR",
    }
)


def expand_path_completion_environment(token: str) -> Optional[tuple[str, str, str]]:
    """Expand one approved environment variable without evaluating shell text."""
    match = re.fullmatch(r"\$(?:\{([A-Za-z_][A-Za-z0-9_]*)\}|([A-Za-z_][A-Za-z0-9_]*))(.*)", token)
    if match is None:
        return None
    name = match.group(1) or match.group(2)
    suffix = match.group(3)
    if name not in PATH_COMPLETION_ENV_VARS or (suffix and not suffix.startswith("/")):
        return None
    value = os.environ.get(name)
    if not value:
        return None
    variable_text = token[: len(token) - len(suffix)] if suffix else token
    return value + suffix, variable_text, suffix


def runtime_completion_matches(
    query: str,
    cursor_pos: int,
    startup_entries: Optional[tuple[DirectoryListingEntry, ...]] = None,
    *,
    cwd: Path,
    limit: int,
) -> list[MatchResult]:
    if limit <= 0:
        return []
    if not query.strip():
        return []

    start, end = token_bounds(query, cursor_pos)
    raw_token = query[start:end]
    quote, _closes_quote = enclosing_quote(raw_token)
    stripped = strip_enclosing_quotes(raw_token)
    if not stripped:
        return []

    # A trailing backslash is an incomplete escape while the user is typing.
    # Ignore it for matching so path completions do not briefly disappear.
    incomplete_escape = stripped.endswith("\\")
    token_prefix = shell_unescape_fragment(stripped[:-1] if incomplete_escape else stripped)
    # An escaped dollar is a literal filename character, not an environment
    # variable reference. Check the raw fragment before unescaping it.
    environment_path = (
        expand_path_completion_environment(token_prefix)
        if quote != "'" and not stripped.startswith("\\$")
        else None
    )
    lookup_prefix = environment_path[0] if environment_path is not None else token_prefix
    chosen_entries: list[DirectoryListingEntry] = []
    completed_prefix = ""

    if "/" in lookup_prefix:
        if lookup_prefix.endswith("/"):
            parent_part = lookup_prefix[:-1]
            name_prefix = ""
        else:
            parent_part, sep, name_prefix = lookup_prefix.rpartition("/")
            if not sep:
                parent_part = ""

        base_dir: Optional[Path] = None
        display_prefix = parent_part
        if environment_path is not None:
            _expanded_value, variable_text, suffix = environment_path
            if suffix.endswith("/"):
                display_prefix = variable_text + suffix[:-1]
            else:
                suffix_parent, _separator, _suffix_name = suffix.rpartition("/")
                display_prefix = variable_text + suffix_parent
            base_dir = Path(parent_part) if parent_part else Path("/")
        elif lookup_prefix.startswith("/"):
            base_dir = Path(parent_part) if parent_part else Path("/")
            display_prefix = parent_part if parent_part else "/"
        elif lookup_prefix.startswith("~"):
            expanded = Path(parent_part if parent_part else "~").expanduser()
            base_dir = expanded
            display_prefix = parent_part if parent_part else "~"
        else:
            rel_parent = Path(parent_part) if parent_part else Path(".")
            base_dir = (cwd / rel_parent).resolve()
            display_prefix = parent_part

        cached_entries = cached_directory_listing(base_dir) if base_dir is not None else None
        if cached_entries is None:
            return []

        visible_entries: list[DirectoryListingEntry] = []
        for entry in cached_entries:
            if entry.name.startswith(".") and not name_prefix.startswith("."):
                continue
            visible_entries.append(entry)

        chosen_entries = top_ranked_directory_entries(name_prefix, tuple(visible_entries))

        if display_prefix == "":
            completed_prefix = ""
        elif display_prefix == ".":
            completed_prefix = "./"
        elif display_prefix == "/":
            completed_prefix = "/"
        elif display_prefix == "~":
            completed_prefix = "~/"
        else:
            completed_prefix = display_prefix.rstrip("/") + "/"
    else:
        if len(token_prefix) <= 2:
            return []
        entries = startup_entries if startup_entries is not None else cached_directory_listing(cwd)
        if entries is None:
            return []
        token_prefix_lower = token_prefix.lower()
        matches: list[DirectoryListingEntry] = []
        for entry in entries:
            if entry.name.startswith(".") and not token_prefix.startswith("."):
                continue
            if entry.name.lower().startswith(token_prefix_lower):
                matches.append(entry)
        if not matches:
            return []
        matches.sort(key=lambda entry: (entry.name.lower(), entry.name))
        chosen_entries = matches

    runtime_matches: list[MatchResult] = []
    for chosen in chosen_entries:
        completed_value = completed_prefix + chosen.name + ("/" if chosen.is_dir else "")
        if environment_path is not None:
            _expanded_value, variable_text, _suffix = environment_path
            suffix = completed_value[len(variable_text) :]
            if quote is not None:
                completed_token = quote + variable_text + shell_escape_quoted_fragment(suffix, quote) + quote
            else:
                completed_token = variable_text + shell_escape_fragment(suffix)
        elif quote is not None:
            completed_token = quote + shell_escape_quoted_fragment(completed_value, quote)
            completed_token += quote
        else:
            completed_token = shell_escape_fragment(completed_value)
        completed_query = replace_query_token(query, cursor_pos, completed_token)
        if completed_query == query:
            continue
        encoded_name = (
            shell_escape_quoted_fragment(chosen.name, quote)
            if quote is not None
            else shell_escape_fragment(chosen.name)
        )
        completion_end = start + len(completed_token) - int(chosen.is_dir) - int(quote is not None)
        completion_start = completion_end - len(encoded_name)

        completed_query_lower = completed_query.lower()
        runtime_matches.append(
            MatchResult(
                text=completed_query,
                score=10**9,
                text_lower=completed_query_lower,
                runtime_completion=True,
                runtime_completion_span=(completion_start, completion_end),
            )
        )
        if len(runtime_matches) >= limit:
            break

    return runtime_matches


def insert_runtime_completions(
    results: list[MatchResult],
    runtime_completions: list[MatchResult],
    *,
    featured_count: int,
) -> list[MatchResult]:
    if not runtime_completions:
        return results
    merged = list(results)
    runtime_spans = {item.text: item.runtime_completion_span for item in runtime_completions}
    for index, item in enumerate(merged):
        if item.text in runtime_spans:
            merged[index] = replace(
                item,
                runtime_completion=True,
                runtime_completion_span=runtime_spans[item.text],
            )
    merged_texts = {item.text for item in merged}
    insertion_index = 0
    for runtime_completion in runtime_completions[:featured_count]:
        if runtime_completion.text in merged_texts:
            continue
        merged.insert(insertion_index, runtime_completion)
        merged_texts.add(runtime_completion.text)
        insertion_index += 1
    for runtime_completion in runtime_completions[featured_count:]:
        if runtime_completion.text in merged_texts:
            continue
        merged.append(runtime_completion)
        merged_texts.add(runtime_completion.text)
    return merged


def shell_words_for_matching(text: str) -> list[str]:
    stripped = text.strip().lower()
    if not stripped:
        return []
    try:
        tokens = shlex.split(stripped)
    except ValueError:
        tokens = stripped.split()
    return [token for token in tokens if token]


def dedupe_match_results_preserving_order(results: list[MatchResult]) -> list[MatchResult]:
    deduped: list[MatchResult] = []
    seen: set[str] = set()
    for item in results:
        if item.text in seen:
            continue
        seen.add(item.text)
        deduped.append(item)
    return deduped


def native_history_candidate_inputs(history: Sequence[HistoryEntry]) -> list[tuple[Any, ...]]:
    return [
        (
            entry.text,
            entry.text_lower or entry.text.lower(),
            entry.cwd,
            list(
                entry.words
                or tuple(shell_words_for_matching(entry.text_lower or entry.text.lower()))
            ),
            entry.failed,
        )
        for entry in history
    ]


def build_native_history_candidates(history: Sequence[HistoryEntry]) -> Any:
    """Copy searchable history text into a Rust-owned cache once per load."""
    return _NativeHistory(native_history_candidate_inputs(history))


@dataclass(frozen=True)
class CustomHistoryWatermark:
    row_id: int
    entry: HistoryEntry


def custom_history_records_from_rows(
    rows: Sequence[object],
) -> list[CustomHistoryWatermark]:
    records: list[CustomHistoryWatermark] = []
    for row in rows:
        if not isinstance(row, tuple) or len(row) < 5 or not isinstance(row[0], int):
            continue
        entry = custom_history_entry_from_row(row[1:])
        if entry is not None:
            records.append(CustomHistoryWatermark(row[0], entry))
    return records


def build_native_custom_history_candidates(
    path: Path,
    *,
    limit: Optional[int] = None,
    batch_size: int = 1_000,
) -> tuple[Any, Optional[CustomHistoryWatermark], int]:
    """Stream SQLite history into Rust without retaining a full Python copy."""
    native_candidates = _NativeHistory([])
    if not path.exists():
        return native_candidates, None, 0

    query = "SELECT id, command, cwd, timestamp, failed FROM custom_history ORDER BY id DESC"
    params: tuple[object, ...] = ()
    if limit is not None and limit > 0:
        query += " LIMIT ?"
        params = (limit,)
    watermark: Optional[CustomHistoryWatermark] = None
    status_revision = 0
    try:
        with sqlite3.connect(path) as conn:
            revision_row = conn.execute(
                "SELECT status_revision FROM custom_history_metadata WHERE id = 1"
            ).fetchone()
            if revision_row and isinstance(revision_row[0], int):
                status_revision = revision_row[0]
            cursor = conn.execute(query, params)
            while True:
                rows = cursor.fetchmany(batch_size)
                if not rows:
                    break
                records = custom_history_records_from_rows(rows)
                if watermark is None and records:
                    watermark = records[0]
                entries = [record.entry for record in records]
                native_candidates.extend(native_history_candidate_inputs(entries))
    except (OSError, sqlite3.Error):
        return _NativeHistory([]), None, 0
    return native_candidates, watermark, status_revision


def native_history_response_frame(
    query: str,
    native_candidates: Any,
    *,
    candidate_indices: Optional[Sequence[int]] = None,
    limit: Optional[int] = None,
    current_cwd: Optional[str] = None,
) -> bytes:
    """Encode a complete search response as a binary daemon frame."""
    result_limit = limit if (limit is None or limit > 0) else None
    return native_candidates.search_response_frame(
        query.lower(),
        query.strip().lower(),
        shell_words_for_matching(query),
        query.lower().split(),
        current_cwd,
        candidate_indices,
        result_limit,
        MAX_CACHED_CANDIDATE_INDICES,
    )


def native_history_response_frame_for_daemon(
    query: str,
    native_candidates: Any,
    *,
    candidate_indices: Optional[Sequence[int]] = None,
    limit: Optional[int] = None,
    current_cwd: Optional[str] = None,
) -> bytes:
    """Encode results as a binary frame and retain complete matches in-process."""
    result_limit = limit if (limit is None or limit > 0) else None
    return native_candidates.search_response_frame_for_daemon(
        query.lower(),
        query.strip().lower(),
        shell_words_for_matching(query),
        query.lower().split(),
        current_cwd,
        candidate_indices,
        result_limit,
    )


@dataclass
class DaemonHistoryState:
    path: Path
    use_custom_history: bool
    history_length: Optional[int]
    native_candidates: Any
    custom_history_watermark: Optional[CustomHistoryWatermark]
    custom_history_status_revision: int = 0

    @classmethod
    def load(
        cls,
        path: Path,
        *,
        use_custom_history: bool,
        history_length: Optional[int],
    ) -> "DaemonHistoryState":
        if use_custom_history:
            native_candidates, watermark, status_revision = build_native_custom_history_candidates(
                path,
                limit=history_length,
            )
            log_database_load_event(
                "INITIAL_LOAD",
                f"path={path} rows={len(native_candidates)} use_custom_history=True",
            )
            return cls(
                path,
                use_custom_history,
                history_length,
                native_candidates,
                watermark,
                status_revision,
            )

        history = load_history_source(
            path,
            use_custom_history=use_custom_history,
            history_length=history_length if use_custom_history else None,
        )
        native_candidates = build_native_history_candidates(history)
        log_database_load_event(
            "INITIAL_LOAD",
            f"path={path} rows={len(native_candidates)} use_custom_history=False",
        )
        return cls(
            path,
            use_custom_history,
            history_length,
            native_candidates,
            None,
        )

    def __len__(self) -> int:
        return len(self.native_candidates)

    def _rebuild_native(self, reason: str = "unknown") -> None:
        if self.use_custom_history:
            native_candidates, watermark, status_revision = build_native_custom_history_candidates(
                self.path,
                limit=self.history_length,
            )
            self.native_candidates = native_candidates
            self.custom_history_watermark = watermark
            self.custom_history_status_revision = status_revision
            log_database_load_event(
                "FULL_REBUILD",
                f"path={self.path} rows={len(native_candidates)} reason={reason}",
            )
            return

        history = load_history_source(
            self.path,
            use_custom_history=self.use_custom_history,
        )
        self.native_candidates = build_native_history_candidates(history)
        self.custom_history_watermark = None
        self.custom_history_status_revision = 0
        log_database_load_event(
            "FULL_REBUILD",
            f"path={self.path} rows={len(self.native_candidates)} reason={reason}",
        )

    def refresh(self) -> None:
        self.native_candidates.clear_daemon_query_cache()
        if not self.use_custom_history:
            self._rebuild_native("non_custom_history")
            return

        watermark = self.custom_history_watermark
        if watermark is None:
            self._rebuild_native("no_watermark")
            return

        try:
            with sqlite3.connect(self.path) as conn:
                # Keep the row changes and revision cursor on one SQLite snapshot.
                conn.execute("BEGIN")
                rows = conn.execute(
                    """
                    SELECT id, command, cwd, timestamp, failed
                    FROM custom_history
                    WHERE id >= ?
                    ORDER BY id DESC
                    """,
                    (watermark.row_id,),
                ).fetchall()
                status_rows = conn.execute(
                    """
                    SELECT h.failed,
                           (SELECT COUNT(*) FROM custom_history AS newer WHERE newer.id > h.id)
                    FROM custom_history AS h
                    WHERE h.status_revision > ?
                    ORDER BY h.status_revision
                    """,
                    (self.custom_history_status_revision,),
                ).fetchall()
                revision_row = conn.execute(
                    "SELECT status_revision FROM custom_history_metadata WHERE id = 1"
                ).fetchone()
        except (OSError, sqlite3.Error) as exc:
            self._rebuild_native(f"sqlite_error:{exc}")
            return

        records = custom_history_records_from_rows(rows)
        anchor = next(
            (record for record in records if record.row_id == watermark.row_id),
            None,
        )
        watermark_replaced = False
        if anchor is None and records:
            watermark_replaced = any(
                record.row_id > watermark.row_id
                and record.entry.text == watermark.entry.text
                and record.entry.cwd == watermark.entry.cwd
                for record in records
            )

        if (anchor is None and not watermark_replaced) or (
            anchor is not None
            and (
                anchor.entry.text,
                anchor.entry.cwd,
                anchor.entry.timestamp,
            )
            != (
                watermark.entry.text,
                watermark.entry.cwd,
                watermark.entry.timestamp,
            )
        ):
            self._rebuild_native("anchor_missing_or_watermark_modified")
            return

        changed_entries = [
            record.entry for record in records if record.row_id > watermark.row_id
        ]
        if changed_entries:
            self.native_candidates.prepend_replacing(
                native_history_candidate_inputs(changed_entries)
            )
            if self.history_length is not None:
                self.native_candidates.truncate(self.history_length)
        for failed, candidate_index in status_rows:
            if not isinstance(candidate_index, int):
                continue
            if self.history_length is not None and candidate_index >= self.history_length:
                continue
            self.native_candidates.update_failed_at(candidate_index, bool(failed))
        self.custom_history_watermark = records[0] if records else anchor
        if revision_row and isinstance(revision_row[0], int):
            self.custom_history_status_revision = revision_row[0]

    def search_response(
        self,
        query: str,
        *,
        candidate_indices: Optional[Sequence[int]],
        limit: Optional[int],
        current_cwd: Optional[str],
    ) -> bytes:
        if candidate_indices is not None:
            return native_history_response_frame(
                query,
                self.native_candidates,
                candidate_indices=candidate_indices,
                limit=limit,
                current_cwd=current_cwd,
            )

        return native_history_response_frame_for_daemon(
            query,
            self.native_candidates,
            limit=limit,
            current_cwd=current_cwd,
        )


def search_history_ranked_native(
    query: str,
    history: list[HistoryEntry],
    native_candidates: Any,
    *,
    candidate_indices: Optional[Sequence[int]] = None,
    limit: Optional[int] = None,
    current_cwd: Optional[str] = None,
) -> tuple[list[MatchResult], Optional[list[int]]]:
    """Run matching and result ordering in the Rust-owned history cache."""
    if len(native_candidates) != len(history):
        raise ValueError("native and Python history lengths differ")
    result_limit = limit if (limit is None or limit > 0) else None
    selected, matched_indices = native_candidates.search_ranked(
        query.lower(),
        query.strip().lower(),
        shell_words_for_matching(query),
        query.lower().split(),
        current_cwd,
        candidate_indices,
        result_limit,
        MAX_CACHED_CANDIDATE_INDICES,
    )
    results: list[MatchResult] = []
    for idx, score in selected:
        entry = history[idx]
        results.append(
            MatchResult(
                entry.text,
                score,
                exact=False,
                recency=-idx,
                cwd=entry.cwd,
                text_lower=entry.text_lower,
                failed=entry.failed,
                words=entry.words,
            )
        )
    return results, matched_indices


def search_history_response_frame_native(
    query: str,
    history: list[HistoryEntry],
    native_candidates: Any,
    *,
    candidate_indices: Optional[Sequence[int]] = None,
    limit: Optional[int] = None,
    current_cwd: Optional[str] = None,
) -> bytes:
    """Return a complete daemon search response encoded inside Rust."""
    if len(native_candidates) != len(history):
        raise ValueError("native and Python history lengths differ")
    return native_history_response_frame(
        query,
        native_candidates,
        candidate_indices=candidate_indices,
        limit=limit,
        current_cwd=current_cwd,
    )


def search_history_only(
    query: str,
    history: list[HistoryEntry],
    *,
    candidate_indices: Optional[Sequence[int]] = None,
    limit: Optional[int] = None,
    native_candidates: Optional[Any] = None,
) -> tuple[list[MatchResult], list[int]]:
    candidates: range | Sequence[int]
    if candidate_indices is None:
        candidates = range(len(history))
    else:
        candidates = candidate_indices
    result_limit = limit if (limit is None or limit > 0) else None
    if not query:
        results: list[MatchResult] = []
        if result_limit is None:
            source = candidates
        elif candidate_indices is None:
            source = range(min(result_limit, len(history)))
        else:
            source = candidate_indices[:result_limit]
        for idx in source:
            entry = history[idx]
            results.append(
                MatchResult(
                    entry.text,
                    0,
                    exact=False,
                    recency=-idx,
                    cwd=entry.cwd,
                    text_lower=entry.text_lower,
                    failed=entry.failed,
                    words=entry.words,
                )
            )
        if candidate_indices is None:
            return results, list(range(len(history)))
        return results, candidate_indices

    matched_indices: list[int] = []
    history_results: list[MatchResult] = []
    if (
        native_candidates is not None
        and len(native_candidates) == len(history)
    ):
        native_matches = native_candidates.flex_match_many(query.lower(), candidate_indices)
        for idx, score in native_matches:
            entry = history[idx]
            cmd = entry.text
            if query_equals_candidate(query, cmd):
                continue
            matched_indices.append(idx)
            history_results.append(
                MatchResult(
                    cmd,
                    score,
                    exact=False,
                    recency=-idx,
                    cwd=entry.cwd,
                    text_lower=entry.text_lower,
                    failed=entry.failed,
                    words=entry.words,
                )
            )
    else:
        for idx in candidates:
            entry = history[idx]
            cmd = entry.text
            m = flex_match(query, cmd, candidate_lower=entry.text_lower)
            if m is None:
                continue
            if query_equals_candidate(query, cmd):
                continue

            matched_indices.append(idx)

            m.exact = query_equals_candidate(query, cmd)
            m.recency = -idx
            m.cwd = entry.cwd
            m.text_lower = entry.text_lower
            m.failed = entry.failed
            m.words = entry.words
            history_results.append(m)

    if result_limit is not None:
        history_results = history_results[:result_limit]
    return history_results, matched_indices


def prefer_current_cwd(
    results: list[MatchResult],
    *,
    current_cwd: Optional[str],
) -> list[MatchResult]:
    if not current_cwd:
        return list(results)
    same_cwd: list[MatchResult] = []
    other: list[MatchResult] = []
    for item in results:
        if item.cwd == current_cwd:
            same_cwd.append(item)
        else:
            other.append(item)
    return same_cwd + other


def query_words_appear_in_order(query: str, text_lower: str) -> bool:
    words = query.lower().split()
    if not words:
        return False

    at = 0
    for word in words:
        idx = text_lower.find(word, at)
        if idx == -1:
            return False
        at = idx + len(word)
    return True


def apply_inner_bucket_priority(
    query: str,
    results: list[MatchResult],
    *,
    current_cwd: Optional[str],
) -> list[MatchResult]:
    words_in_order: list[MatchResult] = []
    rest: list[MatchResult] = []
    for item in results:
        text_lower = item.text_lower if item.text_lower is not None else item.text.lower()
        if query_words_appear_in_order(query, text_lower):
            words_in_order.append(item)
        else:
            rest.append(item)
    return prefer_current_cwd(words_in_order, current_cwd=current_cwd) + prefer_current_cwd(
        rest,
        current_cwd=current_cwd,
    )


def apply_prefix_priority(
    query: str,
    results: list[MatchResult],
    *,
    limit: Optional[int] = None,
    current_cwd: Optional[str] = None,
) -> list[MatchResult]:
    result_limit = limit if (limit is None or limit > 0) else None
    if not results:
        return results

    query_words = shell_words_for_matching(query)
    prefix_word_counts: list[int] = []
    max_prefix_words = 0
    if query_words:
        for item in results:
            text_lower = item.text_lower if item.text_lower is not None else item.text.lower()
            candidate_words = item.words or shell_words_for_matching(text_lower)
            matched_words = 0
            for query_word, candidate_word in zip(query_words, candidate_words):
                if not candidate_word.startswith(query_word):
                    break
                matched_words += 1
            prefix_word_counts.append(matched_words)
            if matched_words > max_prefix_words:
                max_prefix_words = matched_words
    else:
        prefix_word_counts = [0] * len(results)

    if max_prefix_words > 0:
        tier_prefix: list[MatchResult] = []
        tier_rest: list[MatchResult] = []
        for item, prefix_word_count in zip(results, prefix_word_counts):
            if prefix_word_count == max_prefix_words:
                tier_prefix.append(item)
            else:
                tier_rest.append(item)
        ordered_results = apply_inner_bucket_priority(
            query,
            tier_prefix,
            current_cwd=current_cwd,
        ) + apply_inner_bucket_priority(
            query,
            tier_rest,
            current_cwd=current_cwd,
        )
    else:
        ordered_results = apply_inner_bucket_priority(query, results, current_cwd=current_cwd)
    ordered_results = dedupe_match_results_preserving_order(ordered_results)
    if result_limit is not None:
        return ordered_results[:result_limit]
    return ordered_results


def search(
    query: str,
    history: list[HistoryEntry],
    *,
    cursor_pos: int = 0,
    candidate_indices: Optional[Sequence[int]] = None,
    limit: Optional[int] = None,
    cwd: Optional[Path] = None,
    native_candidates: Optional[Any] = None,
) -> tuple[list[MatchResult], list[int]]:
    history_results, matched_indices = search_history_only(
        query,
        history,
        candidate_indices=candidate_indices,
        native_candidates=native_candidates,
    )
    current_cwd = normalize_cwd_value(str(cwd)) if cwd is not None else None
    results = apply_prefix_priority(
        query,
        history_results,
        limit=limit,
        current_cwd=current_cwd,
    )
    return results, matched_indices


def launch_history_daemon(
    script_path: Path,
    history_path: Path,
    socket_path: Path,
    *,
    history_length: Optional[int],
    use_custom_history: bool = False,
) -> bool:
    try:
        socket_path.parent.mkdir(parents=True, exist_ok=True)
    except OSError:
        return False

    cmd = [
        sys.executable,
        "-m",
        "zsh_flex_history.cli",
        "--daemon",
        "--history-file",
        str(history_path),
        "--socket-path",
        str(socket_path),
    ]
    if history_length is not None:
        cmd.extend(["--history-length", str(history_length)])
    if use_custom_history:
        cmd.append("--use-custom-history")
    try:
        subprocess.Popen(
            cmd,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            start_new_session=True,
            close_fds=True,
        )
    except OSError:
        return False
    return True


class HistoryDaemonClient:
    def __init__(
        self,
        socket_path: Path,
        history_path: Path,
        script_path: Path,
        *,
        debug: bool = False,
        history_length: Optional[int] = None,
        use_custom_history: bool = False,
    ) -> None:
        self.socket_path = socket_path
        self.history_path = history_path
        self.script_path = script_path
        self.debug = debug
        self.history_length = history_length
        self.use_custom_history = use_custom_history

    def ensure_running(self) -> bool:
        if _native_ping_daemon(str(self.socket_path), 0.15):
            daemon_debug_log(self.debug, f"using existing daemon at {self.socket_path}")
            return True

        if self.socket_path.exists():
            try:
                self.socket_path.unlink()
                daemon_debug_log(self.debug, f"removed stale socket at {self.socket_path}")
            except OSError:
                pass

        daemon_debug_log(self.debug, f"starting new daemon at {self.socket_path}")
        if not launch_history_daemon(
            self.script_path,
            self.history_path,
            self.socket_path,
            history_length=self.history_length,
            use_custom_history=self.use_custom_history,
        ):
            daemon_debug_log(self.debug, "failed to launch daemon process")
            return False

        deadline = time.monotonic() + 1.0
        while time.monotonic() < deadline:
            if _native_ping_daemon(str(self.socket_path), 0.15):
                daemon_debug_log(self.debug, "new daemon is ready")
                return True
            time.sleep(0.03)
        daemon_debug_log(self.debug, "daemon did not become ready before timeout")
        return False

    def search_history(
        self,
        query: str,
        *,
        candidate_indices: Optional[Sequence[int]] = None,
        limit: Optional[int] = None,
        cwd: Optional[str] = None,
    ) -> Optional[tuple[list[MatchResult], Optional[list[int]]]]:
        bounded_indices = (
            candidate_indices
            if candidate_indices is not None
            and len(candidate_indices) <= MAX_CACHED_CANDIDATE_INDICES
            else None
        )
        normalized_cwd = normalize_cwd_value(cwd) if cwd else None
        exchanged, parsed_native = _native_search_daemon(
            str(self.socket_path),
            query,
            bounded_indices,
            limit,
            normalized_cwd,
        )
        if not exchanged:
            if not self.ensure_running():
                return None
            exchanged, parsed_native = _native_search_daemon(
                str(self.socket_path),
                query,
                bounded_indices,
                limit,
                normalized_cwd,
            )
            if not exchanged:
                return None
        if parsed_native is None:
            return None

        raw_results, parsed_indices = parsed_native
        parsed_results = [
            MatchResult(
                text=text,
                score=score,
                exact=exact,
                recency=recency,
                cwd=result_cwd,
                failed=failed,
                words=tuple(words),
            )
            for text, score, exact, recency, result_cwd, failed, words in raw_results
        ]
        if self.debug:
            indices_state = "included" if parsed_indices is not None else "omitted"
            daemon_debug_log(
                True,
                f"query={query!r} matched_indices={indices_state}",
            )
        return parsed_results, parsed_indices


def run_history_daemon(
    history_path: Path,
    socket_path: Path,
    *,
    debug: bool = False,
    history_length: Optional[int] = None,
    use_custom_history: bool = False,
) -> int:
    if use_custom_history and not history_path.exists():
        try:
            ensure_custom_history_file(history_path)
        except OSError as exc:
            print(f"zsh_flex_history daemon: failed to initialize custom history: {exc}", file=sys.stderr)
            return 1
    history_state = DaemonHistoryState.load(
        history_path,
        use_custom_history=use_custom_history,
        history_length=history_length if use_custom_history else None,
    )
    signature = history_file_signature(history_path)

    try:
        socket_path.parent.mkdir(parents=True, exist_ok=True)
    except OSError as exc:
        print(f"zsh_flex_history daemon: failed to create socket directory: {exc}", file=sys.stderr)
        return 1

    if socket_path.exists():
        if _native_ping_daemon(str(socket_path), 0.15):
            daemon_debug_log(debug, f"daemon already running at {socket_path}, exiting")
            return 0
        try:
            socket_path.unlink()
            daemon_debug_log(debug, f"removed stale socket at {socket_path}")
        except OSError:
            pass

    try:
        native_server = _NativeDaemonServer(str(socket_path))
    except OSError as exc:
        print(f"zsh_flex_history daemon: failed to bind socket: {exc}", file=sys.stderr)
        return 1
    daemon_debug_log(debug, f"daemon listening on {socket_path} (history={history_path})")
    try:
        while True:
            try:
                request = native_server.accept_search()
            except OSError:
                continue

            new_signature = history_file_signature(history_path)
            if new_signature != signature:
                history_state.refresh()
                signature = new_signature

            raw_candidates = request.candidate_indices
            candidate_indices = None
            if raw_candidates is not None:
                max_idx = len(history_state) - 1
                candidate_indices = [idx for idx in raw_candidates if idx <= max_idx]
            current_cwd = normalize_cwd_value(request.cwd) if request.cwd is not None else None
            response = history_state.search_response(
                request.query,
                candidate_indices=candidate_indices,
                limit=request.limit,
                current_cwd=current_cwd,
            )
            request.respond_frame(response)
    finally:
        try:
            socket_path.unlink()
        except OSError:
            pass
