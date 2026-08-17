#!/usr/bin/env python3
"""Benchmark in-memory history searches with synthetic and read-only real data.

Run from the repository root:

    uv run python tests/benchmark_search.py

Each invocation benchmarks both a fixed-seed corpus and a fresh random-seed
corpus. It combines common shell commands, synthetic project paths,
random-looking identifiers, varying command lengths, and occasional long
command lines. Only calls to ``engine.search`` are timed; corpus construction
is deliberately excluded.
"""

from __future__ import annotations

import json
import random
import secrets
import sqlite3
import statistics
import sys
import time
from string import ascii_lowercase
from pathlib import Path
from typing import Optional


ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "src"))

from zsh_flex_history import engine
from zsh_flex_history.engine import (
    HistoryEntry,
    default_custom_history_path,
    make_history_entry,
    normalize_cwd_value,
    search,
)


SIZES = (1_000, 5_000, 10_000, 20_000, 40_000, 60_000, 80_000, 100_000)
FIXED_SEED = 20_260_817
QUERIES = (
    "git st",
    "python -m",
    "kub get",
    "docker com",
    "npm run",
    "cd src",
    "rg --",
    "deploy prod",
)
SNAPSHOT_QUERIES = QUERIES + ("tool dep", "worker dis")
SNAPSHOT_SIZE = 100_000
SNAPSHOT_DIRECTORY = ROOT / "tests" / "benchmark_results"
SINGLE_LETTER_QUERIES = tuple(random.Random(20260817).sample(ascii_lowercase, 3))
COMMON_COMMANDS = (
    "git status --short",
    "git switch main",
    "git commit -am 'update implementation'",
    "python -m pytest -q",
    "python -m pip install -e .",
    "kubectl get pods --all-namespaces",
    "docker compose up --build",
    "npm run test -- --watch=false",
    "rg --hidden --glob '!node_modules' TODO src",
    "cd src/zsh_flex_history",
    "deploy production --region us-central1",
)
VERBS = ("build", "check", "deploy", "format", "lint", "migrate", "serve", "test")
TARGETS = ("api", "backend", "cli", "docs", "frontend", "worker")


CharacterPresenceIndex = Optional[dict[str, bytearray]]


def build_optional_character_presence_index(history: list[HistoryEntry]) -> CharacterPresenceIndex:
    """Use the index when this revision provides it; otherwise use old search."""
    builder = getattr(engine, "build_character_presence_index", None)
    return builder(history) if builder is not None else None


def benchmark_search(
    query: str,
    history: list[HistoryEntry],
    character_presence_index: CharacterPresenceIndex,
) -> tuple[list[object], object]:
    if character_presence_index is None:
        return search(query, history, limit=100)
    return search(query, history, limit=100, character_presence_index=character_presence_index)


def synthetic_command(index: int, rng: random.Random) -> str:
    """Return a repeatable mix of short, ordinary, and long shell commands."""
    if index % 11 == 0:
        return COMMON_COMMANDS[index % len(COMMON_COMMANDS)]

    verb = VERBS[index % len(VERBS)]
    target = TARGETS[(index // len(VERBS)) % len(TARGETS)]
    identifier = f"{rng.getrandbits(48):012x}"
    path = f"projects/{target}/src/module_{index % 211}/file_{index % 97}.py"
    command = f"tool {verb} {target} --request-id {identifier} --path {path}"

    if index % 17 == 0:
        command += f" --tag {rng.choice(('alpha', 'beta', 'canary', 'stable'))}"
    if index % 97 == 0:
        payload = " ".join(f"key_{part}={rng.getrandbits(32):08x}" for part in range(180))
        command = f"python -m worker.dispatch --target {target} --payload '{payload}'"
    return command


def build_history(size: int, seed: int) -> list[HistoryEntry]:
    rng = random.Random(seed + size)
    return [make_history_entry(synthetic_command(index, rng)) for index in range(size)]


def repetitions_for(size: int) -> int:
    if size <= 10_000:
        return 7
    if size <= 40_000:
        return 4
    return 3


def benchmark(
    history: list[HistoryEntry],
    character_presence_index: CharacterPresenceIndex,
    queries: tuple[str, ...],
    repetitions: int,
) -> tuple[float, int]:
    # Warm caches and validate that every query produces a normal result list.
    for query in queries:
        results, _ = benchmark_search(query, history, character_presence_index)
        assert isinstance(results, list)

    samples: list[float] = []
    result_count = 0
    for _ in range(repetitions):
        started = time.perf_counter()
        for query in queries:
            results, _ = benchmark_search(query, history, character_presence_index)
            result_count += len(results)
        samples.append(time.perf_counter() - started)
    return statistics.median(samples) * 1_000 / len(queries), result_count


def snapshot_results(
    history: list[HistoryEntry],
    character_presence_index: CharacterPresenceIndex,
    seed: int,
) -> dict[str, object]:
    return {
        "seed": seed,
        "history_size": len(history),
        "queries": [
            {
                "query": query,
                "results": [
                    item.text
                    for item in benchmark_search(query, history, character_presence_index)[0]
                ],
            }
            for query in SNAPSHOT_QUERIES
        ],
    }


def write_fixed_snapshot(snapshot: dict[str, object]) -> Path:
    SNAPSHOT_DIRECTORY.mkdir(parents=True, exist_ok=True)
    timestamp = time.strftime("%Y%m%d-%H%M%S", time.gmtime())
    path = SNAPSHOT_DIRECTORY / f"fixed-{timestamp}.txt"
    path.write_text(json.dumps(snapshot, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return path


def compare_fixed_snapshots(snapshot: dict[str, object], current_path: Path) -> list[Path]:
    differences: list[Path] = []
    for path in SNAPSHOT_DIRECTORY.glob("fixed-*.txt"):
        if path == current_path:
            continue
        try:
            previous = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            differences.append(path)
            continue
        if previous != snapshot:
            differences.append(path)
    return sorted(differences)


def benchmark_run(label: str, seed: int) -> list[HistoryEntry]:
    letters = ", ".join(SINGLE_LETTER_QUERIES)
    print(f"\n{label} seed: {seed}")
    print(f"Single-letter sample: {letters}")
    print("entries  multiword ms/search  single-letter ms/search  repetitions")
    largest_history: list[HistoryEntry] = []
    for size in SIZES:
        history = build_history(size, seed)
        character_presence_index = build_optional_character_presence_index(history)
        repetitions = repetitions_for(size)
        multiword_ms, _ = benchmark(history, character_presence_index, QUERIES, repetitions)
        single_letter_ms, _ = benchmark(history, character_presence_index, SINGLE_LETTER_QUERIES, repetitions)
        print(f"{size:>7,}  {multiword_ms:>19.2f}  {single_letter_ms:>23.2f}  {repetitions:>11}")
        if size == SNAPSHOT_SIZE:
            largest_history = history
    return largest_history


def load_real_history_read_only(path: Path) -> list[HistoryEntry]:
    """Load the custom-history database without allowing SQLite to write to it."""
    if not path.is_file():
        return []

    # ``mode=ro`` prevents SQLite from creating the database or modifying it.
    database_uri = f"{path.resolve().as_uri()}?mode=ro"
    query = "SELECT command, cwd, timestamp, failed FROM custom_history ORDER BY id DESC"
    try:
        with sqlite3.connect(database_uri, uri=True) as connection:
            rows = connection.execute(query).fetchall()
    except (OSError, sqlite3.Error):
        return []

    entries: list[HistoryEntry] = []
    for command, cwd, timestamp, failed in rows:
        if not isinstance(command, str):
            continue
        cleaned = command.replace("\r\n", "\n").replace("\r", "\n").replace("\x00", "").strip("\n")
        if not cleaned.strip():
            continue
        entries.append(
            make_history_entry(
                cleaned,
                cwd=normalize_cwd_value(cwd) if isinstance(cwd, str) else None,
                timestamp=timestamp if isinstance(timestamp, str) else None,
                failed=bool(failed),
            )
        )
    return entries


def benchmark_real_history() -> None:
    """Benchmark the user's actual custom history without daemon involvement."""
    path = default_custom_history_path()
    history = load_real_history_read_only(path)
    if not history:
        print(f"\nReal custom-history benchmark skipped: no readable entries at {path}")
        return

    character_presence_index = build_optional_character_presence_index(history)
    repetitions = repetitions_for(len(history))
    multiword_ms, _ = benchmark(history, character_presence_index, QUERIES, repetitions)
    single_letter_ms, _ = benchmark(history, character_presence_index, SINGLE_LETTER_QUERIES, repetitions)
    print("\nReal custom-history (SQLite read-only; no daemon)")
    print(f"database: {path}")
    print(f"entries: {len(history):,}; repetitions: {repetitions}")
    print(f"multiword ms/search: {multiword_ms:.2f}")
    print(f"single-letter ms/search ({', '.join(SINGLE_LETTER_QUERIES)}): {single_letter_ms:.2f}")


def main() -> int:
    fixed_history = benchmark_run("Fixed", FIXED_SEED)
    snapshot = snapshot_results(
        fixed_history,
        build_optional_character_presence_index(fixed_history),
        FIXED_SEED,
    )
    snapshot_path = write_fixed_snapshot(snapshot)
    differences = compare_fixed_snapshots(snapshot, snapshot_path)
    print(f"\nFixed ordered-results snapshot: {snapshot_path}")
    if differences:
        print("Regression: ordered results differ from:")
        for path in differences:
            print(f"  {path}")
    else:
        print("Regression check: no differences from prior fixed snapshots.")

    benchmark_run("Random", secrets.randbits(63))
    benchmark_real_history()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
