#!/usr/bin/env python3
"""Benchmark in-memory history searches without reading a real database.

Run from the repository root:

    uv run python tests/benchmark_search.py

The generated corpus is deterministic. It combines common shell commands,
synthetic project paths, random-looking identifiers, varying command lengths,
and occasional long command lines. Only calls to ``engine.search`` are timed;
corpus construction is deliberately excluded.
"""

from __future__ import annotations

import random
import statistics
import sys
import time
from string import ascii_lowercase
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "src"))

from zsh_flex_history.engine import HistoryEntry, make_history_entry, search


SIZES = (1_000, 5_000, 10_000, 20_000, 40_000, 60_000, 80_000, 100_000)
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


def build_history(size: int) -> list[HistoryEntry]:
    rng = random.Random(size)
    return [make_history_entry(synthetic_command(index, rng)) for index in range(size)]


def repetitions_for(size: int) -> int:
    if size <= 10_000:
        return 7
    if size <= 40_000:
        return 4
    return 3


def benchmark(
    history: list[HistoryEntry],
    queries: tuple[str, ...],
    repetitions: int,
) -> tuple[float, int]:
    # Warm caches and validate that every query produces a normal result list.
    for query in queries:
        results, _ = search(query, history, limit=100)
        assert isinstance(results, list)

    samples: list[float] = []
    result_count = 0
    for _ in range(repetitions):
        started = time.perf_counter()
        for query in queries:
            results, _ = search(query, history, limit=100)
            result_count += len(results)
        samples.append(time.perf_counter() - started)
    return statistics.median(samples) * 1_000 / len(queries), result_count


def main() -> int:
    letters = ", ".join(SINGLE_LETTER_QUERIES)
    print(f"Single-letter sample: {letters}")
    print("entries  multiword ms/search  single-letter ms/search  repetitions")
    for size in SIZES:
        history = build_history(size)
        repetitions = repetitions_for(size)
        multiword_ms, _ = benchmark(history, QUERIES, repetitions)
        single_letter_ms, _ = benchmark(history, SINGLE_LETTER_QUERIES, repetitions)
        print(f"{size:>7,}  {multiword_ms:>19.2f}  {single_letter_ms:>23.2f}  {repetitions:>11}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
