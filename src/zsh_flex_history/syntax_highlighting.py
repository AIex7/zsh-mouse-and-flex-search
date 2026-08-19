#!/usr/bin/env python3
"""Basic zsh syntax highlighting helpers for interactive query rendering."""

from __future__ import annotations

from bisect import bisect_left
from functools import lru_cache
import os
import re
from typing import Optional

from ._flex_match import NativeIncrementalHighlighter as _NativeIncrementalHighlighter


ANSI_STYLE_BY_TOKEN = {
    "default": "",
    "command": "\x1b[32m",      # green
    "keyword": "\x1b[34m",      # blue
    "option": "\x1b[36m",       # cyan
    "string": "\x1b[33m",       # yellow
    "variable": "\x1b[35m",     # magenta
    "operator": "",             # default
    "comment": "\x1b[90m",      # bright black
    "assignment": "",           # default
    "error": "",                # default (no special error coloring)
}

TOKEN_NAMES = (
    "default",
    "command",
    "keyword",
    "option",
    "string",
    "variable",
    "operator",
    "comment",
    "assignment",
    "error",
)
ANSI_STYLE_BY_TOKEN_ID = tuple(ANSI_STYLE_BY_TOKEN[name] for name in TOKEN_NAMES)


KEYWORDS = {
    "if",
    "then",
    "else",
    "elif",
    "fi",
    "for",
    "while",
    "until",
    "do",
    "done",
    "in",
    "case",
    "esac",
    "select",
    "function",
    "time",
    "coproc",
    "repeat",
    "noglob",
    "builtin",
    "command",
    "exec",
    "eval",
    "source",
    ".",
}


AMBIGUOUS_COMMAND_RE = re.compile(r"[\\$*?[\]{}()'\"`=]")

# Common zsh builtins and shell-resolved command words.
BUILTINS = {
    "alias",
    "autoload",
    "bg",
    "bindkey",
    "break",
    "builtin",
    "bye",
    "cd",
    "chdir",
    "command",
    "compgen",
    "complete",
    "continue",
    "declare",
    "dirs",
    "disable",
    "disown",
    "echo",
    "echotc",
    "emulate",
    "enable",
    "eval",
    "exec",
    "exit",
    "export",
    "false",
    "fc",
    "fg",
    "functions",
    "getopts",
    "hash",
    "history",
    "jobs",
    "kill",
    "let",
    "limit",
    "local",
    "logout",
    "popd",
    "print",
    "printf",
    "pushd",
    "pwd",
    "read",
    "readonly",
    "rehash",
    "return",
    "set",
    "setopt",
    "shift",
    "source",
    "suspend",
    "test",
    "times",
    "trap",
    "true",
    "type",
    "typeset",
    "ulimit",
    "umask",
    "unalias",
    "unfunction",
    "unset",
    "unsetopt",
    "wait",
    "whence",
    "where",
    "which",
    "zmodload",
}


def ansi_for_token(token: str | int) -> str:
    if isinstance(token, int):
        if 0 <= token < len(ANSI_STYLE_BY_TOKEN_ID):
            return ANSI_STYLE_BY_TOKEN_ID[token]
        return ""
    return ANSI_STYLE_BY_TOKEN.get(token, "")


def highlight_tokens(query: str) -> list[str] | bytes:
    """Highlight a complete command line without retaining editor state."""
    return _NativeIncrementalHighlighter().highlight(query, _command_state)


class IncrementalHighlighter:
    """Retain Rust syntax tokens across edits."""

    def __init__(self) -> None:
        self._native = _NativeIncrementalHighlighter()
        self._query: Optional[str] = None
        self._tokens: list[str] | bytes = []

    def highlight(self, query: str) -> list[str] | bytes:
        if query == self._query:
            return self._tokens
        tokens = self._native.highlight(query, _command_state)
        self._query = query
        self._tokens = tokens
        return tokens


def _command_state(word: str, word_complete: bool) -> str:
    # Avoid false positives for ambiguous/incomplete shell forms while typing.
    if not word or AMBIGUOUS_COMMAND_RE.search(word):
        return "pending"

    if _is_valid_command(word):
        return "valid"

    # For in-progress typing, unknown non-prefix commands are likely errors.
    # Keep known prefixes neutral until command token is complete.
    if not word_complete:
        if _is_known_command_prefix(word) or _is_existing_path_prefix(word):
            return "pending"
        return "error"

    if word_complete:
        return "error"
    return "pending"


def _path_cache_cwd(path_env: str) -> str:
    """Include cwd in the cache key only when PATH resolution depends on it."""
    if all(
        path_dir and os.path.isabs(path_dir)
        for path_dir in path_env.split(os.pathsep)
    ):
        return ""
    try:
        return os.getcwd()
    except OSError:
        return ""


@lru_cache(maxsize=16)
def _path_executable_names(path_env: str, cwd: str) -> tuple[str, ...]:
    """Load executable names once for a PATH value and relevant cwd."""
    names: set[str] = set()
    scanned_directories: set[str] = set()
    for path_dir in path_env.split(os.pathsep):
        resolved_dir = path_dir or cwd or "."
        if not os.path.isabs(resolved_dir) and cwd:
            resolved_dir = os.path.join(cwd, resolved_dir)
        if resolved_dir in scanned_directories:
            continue
        scanned_directories.add(resolved_dir)
        try:
            with os.scandir(resolved_dir) as entries:
                for entry in entries:
                    try:
                        if entry.is_file() and os.access(entry.path, os.X_OK):
                            names.add(entry.name)
                    except OSError:
                        continue
        except OSError:
            continue
    return tuple(sorted(names))


def _path_executables(path_env: str) -> tuple[str, ...]:
    return _path_executable_names(path_env, _path_cache_cwd(path_env))


def _sorted_names_contain(names: tuple[str, ...], word: str) -> bool:
    index = bisect_left(names, word)
    return index < len(names) and names[index] == word


def _is_valid_command(word: str) -> bool:
    if word in KEYWORDS or word in BUILTINS:
        return True
    if "/" in word:
        path = os.path.expanduser(word)
        return os.path.isfile(path) and os.access(path, os.X_OK)
    path_env = os.environ.get("PATH", "")
    return _sorted_names_contain(_path_executables(path_env), word)


def _is_known_command_prefix(prefix: str) -> bool:
    if not prefix:
        return False
    if any(k.startswith(prefix) for k in KEYWORDS):
        return True
    if any(b.startswith(prefix) for b in BUILTINS):
        return True
    path_env = os.environ.get("PATH", "")
    return _path_has_prefix(path_env, prefix)


def _is_existing_path_prefix(text: str) -> bool:
    if not text:
        return False
    if "/" not in text and not text.startswith("~"):
        return False

    expanded = os.path.expanduser(text)
    parent_part, sep, name_prefix = expanded.rpartition("/")
    if sep:
        base_dir = parent_part or "/"
    else:
        base_dir = "."
        name_prefix = expanded

    try:
        with os.scandir(base_dir) as entries:
            for entry in entries:
                if entry.name.startswith(name_prefix):
                    return True
    except OSError:
        return False
    return False


def _path_has_prefix(path_env: str, prefix: str) -> bool:
    if not prefix:
        return False
    names = _path_executables(path_env)
    index = bisect_left(names, prefix)
    return index < len(names) and names[index].startswith(prefix)
