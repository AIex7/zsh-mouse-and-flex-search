#!/usr/bin/env python3
"""Benchmark the exact syntax-highlighting work performed while editing.

Run from the repository root after building/installing the native extension:

    uv run python tests/benchmark_syntax_highlighting.py

The benchmark feeds complete editing timelines to fresh incremental highlighters:
character-by-character typing, unchanged-query redraws, backspaces, pasted text,
and mid-line edits. Command lookup caches are warmed before timing. Rendering,
terminal I/O, history searching, and benchmark-data construction are excluded.
Every native timeline is checked against the original Python implementation
before timing.
"""

from __future__ import annotations

import statistics
import sys
import time
from pathlib import Path
from typing import Callable


ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "src"))

from zsh_flex_history import syntax_highlighting


def token_names(tokens: list[str] | bytes) -> list[str]:
    if isinstance(tokens, bytes):
        return [syntax_highlighting.TOKEN_NAMES[token] for token in tokens]
    return tokens


def typing_timeline(command: str) -> list[str]:
    states: list[str] = [""]
    for index in range(1, len(command) + 1):
        query = command[:index]
        states.append(query)
        if index % 7 == 0:
            states.append(query)  # Redraw caused by something other than an edit.

    delete_count = min(12, len(command))
    for count in range(1, delete_count + 1):
        states.append(command[:-count])
    states.append(command)  # Paste/reinsert the deleted suffix.

    midpoint = len(command) // 2
    edited = command[:midpoint] + "--inserted " + command[midpoint:]
    states.append(edited)
    states.append(command)
    return states


TYPICAL_COMMANDS = (
    "git status --short",
    "python -m pytest -q",
    "rg --hidden TODO src",
    "docker compose up --build",
    "kubectl get pods --all-namespaces",
)
COMPLEX_COMMANDS = (
    "FOO=bar command git status && printf '%s\\n' \"$HOME\"",
    "if [[ -n ${DEPLOY_ENV:-} ]]; then deploy production --tag=canary; fi",
    "echo $(git branch --show-current) | rg 'feature|fix' # active branch",
    "value=$((1 + (2 * 3))) && worker --payload '{\"key\":\"éclair\"}'",
)
LONG_COMMAND = (
    "python -m worker.dispatch --target production --payload '"
    + " ".join(f"key_{index}=value_{index:04d}" for index in range(220))
    + "'"
)

SCENARIOS = {
    "typical typing": [state for command in TYPICAL_COMMANDS for state in typing_timeline(command)],
    "shell syntax": [state for command in COMPLEX_COMMANDS for state in typing_timeline(command)],
    "long command": typing_timeline(LONG_COMMAND),
}


def verify_equivalence(states: list[str]) -> None:
    original = syntax_highlighting.PythonIncrementalHighlighter()
    native = syntax_highlighting.IncrementalHighlighter()
    if native._native is None:
        raise RuntimeError("native syntax highlighter is unavailable")
    for query in states:
        expected = original.highlight(query)
        actual = token_names(native.highlight(query))
        if actual != expected:
            raise AssertionError(f"native highlighting differs for {query!r}")


def time_highlighter(
    factory: Callable[[], object],
    states: list[str],
    repetitions: int,
) -> float:
    samples: list[float] = []
    for _ in range(5):
        started = time.perf_counter()
        checksum = 0
        for _ in range(repetitions):
            highlighter = factory()
            for query in states:
                checksum += len(highlighter.highlight(query))
        elapsed = time.perf_counter() - started
        if checksum <= 0:
            raise AssertionError("benchmark did not produce highlighting tokens")
        samples.append(elapsed * 1_000_000 / (repetitions * len(states)))
    return statistics.median(samples)


def main() -> int:
    if syntax_highlighting._NativeIncrementalHighlighter is None:
        print("Native syntax highlighter unavailable; rebuild with `uv run --reinstall-package zsh-flex-history ...`.")
        return 1

    print("Syntax highlighting benchmark (median microseconds per UI highlight call)")
    print("scenario             updates    Python us    Rust us    speedup")
    for label, states in SCENARIOS.items():
        verify_equivalence(states)
        # Keep total processed characters similar across short and long scenarios.
        total_characters = max(1, sum(len(state) for state in states))
        repetitions = max(1, min(50, 200_000 // total_characters))

        # Warm filesystem/PATH command-state caches outside the timed region.
        warm_python = syntax_highlighting.PythonIncrementalHighlighter()
        warm_native = syntax_highlighting.IncrementalHighlighter()
        for query in states:
            warm_python.highlight(query)
            warm_native.highlight(query)

        python_us = time_highlighter(
            syntax_highlighting.PythonIncrementalHighlighter,
            states,
            repetitions,
        )
        rust_us = time_highlighter(
            syntax_highlighting.IncrementalHighlighter,
            states,
            repetitions,
        )
        print(
            f"{label:<20} {len(states):>7,} {python_us:>12.2f} "
            f"{rust_us:>10.2f} {python_us / rust_us:>9.2f}x"
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
