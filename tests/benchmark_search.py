#!/usr/bin/env python3
"""Benchmark the real Rust search engine through its daemon protocol.

Run from the repository root:

    uv run --no-project python tests/benchmark_search.py

The release binary is built from the current checkout before benchmarking.
Corpus generation, SQLite loading, and daemon startup are outside the timed
region. Each timed operation is one end-to-end local daemon request, including
socket exchange and response decoding, handled by the real
``DaemonHistoryState`` and ``NativeHistory`` implementations in
``src/daemon.rs`` and ``src/search.rs``.
"""

from __future__ import annotations

import json
import os
import random
import secrets
import socket
import sqlite3
import statistics
import struct
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path
from string import ascii_lowercase


ROOT = Path(__file__).resolve().parents[1]
RUST_BINARY = ROOT / "target" / "release" / "zsh-flex-history"

FRAME_MAGIC = b"ZFH\x02"
FRAME_PING_REQUEST = 1
FRAME_SEARCH_REQUEST = 2
FRAME_SEARCH_RESPONSE = 0x81
FRAME_PONG_RESPONSE = 0x82
FRAME_ERROR_RESPONSE = 0xFF
MAX_DAEMON_MESSAGE_BYTES = 64 * 1024 * 1024

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


@dataclass(frozen=True)
class HistoryEntry:
    text: str
    cwd: str = ""
    timestamp: str = ""
    failed: bool = False


@dataclass(frozen=True)
class RustMatchResult:
    text: str
    score: int
    exact: bool
    recency: int
    cwd: str | None
    failed: bool
    words: tuple[str, ...]


class FrameReader:
    def __init__(self, payload: bytes) -> None:
        self.payload = payload
        self.position = 0

    def take(self, size: int) -> bytes:
        end = self.position + size
        if size < 0 or end > len(self.payload):
            raise RuntimeError("truncated Rust daemon response")
        value = self.payload[self.position:end]
        self.position = end
        return value

    def byte(self) -> int:
        return self.take(1)[0]

    def boolean(self) -> bool:
        value = self.byte()
        if value not in (0, 1):
            raise RuntimeError("invalid boolean in Rust daemon response")
        return bool(value)

    def u32(self) -> int:
        return struct.unpack("<I", self.take(4))[0]

    def u64(self) -> int:
        return struct.unpack("<Q", self.take(8))[0]

    def i64(self) -> int:
        return struct.unpack("<q", self.take(8))[0]

    def string(self) -> str:
        return self.take(self.u32()).decode("utf-8")

    def optional_string(self) -> str | None:
        return self.string() if self.boolean() else None

    def finish(self) -> None:
        if self.position != len(self.payload):
            raise RuntimeError("trailing bytes in Rust daemon response")


def make_frame(kind: int, payload: bytes = b"") -> bytes:
    body = bytes((kind,)) + payload
    return FRAME_MAGIC + struct.pack("<I", len(body)) + body


def encode_string(value: str) -> bytes:
    encoded = value.encode("utf-8")
    return struct.pack("<I", len(encoded)) + encoded


def search_request(query: str, limit: int = 100) -> bytes:
    payload = b"".join(
        (
            encode_string(query),
            b"\x00",  # No caller-supplied candidate-index bound.
            b"\x01",
            struct.pack("<q", limit),
            b"\x00",  # No current CWD priority for this benchmark.
        )
    )
    return make_frame(FRAME_SEARCH_REQUEST, payload)


def receive_exact(stream: socket.socket, size: int) -> bytes:
    chunks: list[bytes] = []
    remaining = size
    while remaining:
        chunk = stream.recv(remaining)
        if not chunk:
            raise RuntimeError("Rust daemon closed the socket early")
        chunks.append(chunk)
        remaining -= len(chunk)
    return b"".join(chunks)


def exchange(socket_path: Path, request: bytes) -> bytes:
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as stream:
        stream.settimeout(5.0)
        stream.connect(str(socket_path))
        stream.sendall(request)
        header = receive_exact(stream, 8)
        if header[:4] != FRAME_MAGIC:
            raise RuntimeError("invalid frame magic from Rust daemon")
        payload_size = struct.unpack("<I", header[4:])[0]
        if not 0 < payload_size <= MAX_DAEMON_MESSAGE_BYTES:
            raise RuntimeError("invalid frame size from Rust daemon")
        return receive_exact(stream, payload_size)


def parse_search_response(payload: bytes) -> tuple[list[RustMatchResult], list[int] | None]:
    reader = FrameReader(payload)
    kind = reader.byte()
    if kind == FRAME_ERROR_RESPONSE:
        message = reader.string()
        reader.finish()
        raise RuntimeError(f"Rust daemon search error: {message}")
    if kind != FRAME_SEARCH_RESPONSE:
        raise RuntimeError(f"unexpected Rust daemon response kind: {kind:#x}")

    matched_indices = None
    if reader.boolean():
        matched_indices = [reader.u64() for _ in range(reader.u32())]

    results: list[RustMatchResult] = []
    for _ in range(reader.u32()):
        text = reader.string()
        score = reader.i64()
        exact = reader.boolean()
        recency = reader.i64()
        cwd = reader.optional_string()
        failed = reader.boolean()
        words = tuple(reader.string() for _ in range(reader.u32()))
        results.append(RustMatchResult(text, score, exact, recency, cwd, failed, words))
    reader.finish()
    return results, matched_indices


def ensure_rust_binary() -> None:
    print("Building current Rust source in release mode...")
    subprocess.run(
        ["cargo", "build", "--release", "--bin", "zsh-flex-history"],
        cwd=ROOT,
        check=True,
    )


def create_history_database(path: Path, history: list[HistoryEntry]) -> None:
    with sqlite3.connect(path) as connection:
        connection.executescript(
            """
            PRAGMA journal_mode=OFF;
            PRAGMA synchronous=OFF;
            CREATE TABLE custom_history (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                command TEXT NOT NULL,
                cwd TEXT NOT NULL,
                timestamp TEXT NOT NULL,
                failed INTEGER NOT NULL DEFAULT 0,
                status_revision INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE custom_history_metadata (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                status_revision INTEGER NOT NULL DEFAULT 0
            );
            INSERT INTO custom_history_metadata(id, status_revision) VALUES(1, 0);
            """
        )
        # The daemon loads newest row first. Reverse insertion preserves the
        # benchmark list's index 0 == most-recent ordering.
        connection.executemany(
            """
            INSERT INTO custom_history(command, cwd, timestamp, failed)
            VALUES(?, ?, ?, ?)
            """,
            (
                (entry.text, entry.cwd, entry.timestamp, int(entry.failed))
                for entry in reversed(history)
            ),
        )


class RustSearchDaemon:
    def __init__(self, history: list[HistoryEntry]) -> None:
        self._temporary_directory = tempfile.TemporaryDirectory(prefix="zfh-benchmark-")
        directory = Path(self._temporary_directory.name)
        self.database_path = directory / "history.db"
        self.socket_path = directory / "history.sock"
        create_history_database(self.database_path, history)
        self.process = subprocess.Popen(
            [
                str(RUST_BINARY),
                "--daemon",
                "--use-custom-history",
                "--history-file",
                str(self.database_path),
                "--socket-path",
                str(self.socket_path),
                "--history-length",
                str(len(history)),
            ],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
        )
        self._wait_until_ready()

    def _wait_until_ready(self) -> None:
        deadline = time.monotonic() + 10.0
        ping = make_frame(FRAME_PING_REQUEST)
        while time.monotonic() < deadline:
            if self.process.poll() is not None:
                stderr = self.process.communicate()[1].decode("utf-8", errors="replace")
                raise RuntimeError(f"Rust daemon exited during startup: {stderr.strip()}")
            try:
                response = exchange(self.socket_path, ping)
                if response == bytes((FRAME_PONG_RESPONSE,)):
                    return
            except (FileNotFoundError, ConnectionError, OSError, RuntimeError):
                pass
            time.sleep(0.01)
        raise RuntimeError("timed out waiting for the Rust daemon")

    def search(self, query: str) -> tuple[list[RustMatchResult], list[int] | None]:
        return parse_search_response(exchange(self.socket_path, search_request(query)))

    def close(self) -> None:
        if self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=2.0)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()
        if self.process.stderr is not None:
            self.process.stderr.close()
        self._temporary_directory.cleanup()

    def __enter__(self) -> RustSearchDaemon:
        return self

    def __exit__(self, *_: object) -> None:
        self.close()


def benchmark_search(
    query: str,
    native_candidates: RustSearchDaemon,
) -> tuple[list[RustMatchResult], list[int] | None]:
    return native_candidates.search(query)


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
    return [HistoryEntry(synthetic_command(index, rng)) for index in range(size)]


def repetitions_for(size: int) -> int:
    if size <= 10_000:
        return 7
    if size <= 40_000:
        return 4
    return 3


def benchmark(
    native_candidates: RustSearchDaemon,
    queries: tuple[str, ...],
    repetitions: int,
) -> tuple[float, int]:
    for query in queries:
        results, _ = benchmark_search(query, native_candidates)
        assert isinstance(results, list)

    samples: list[float] = []
    result_count = 0
    for _ in range(repetitions):
        started = time.perf_counter()
        for query in queries:
            results, _ = benchmark_search(query, native_candidates)
            result_count += len(results)
        samples.append(time.perf_counter() - started)
    return statistics.median(samples) * 1_000 / len(queries), result_count


def snapshot_results(
    history_size: int,
    native_candidates: RustSearchDaemon,
    seed: int,
) -> dict[str, object]:
    return {
        "seed": seed,
        "history_size": history_size,
        "queries": [
            {
                "query": query,
                "results": [item.text for item in benchmark_search(query, native_candidates)[0]],
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


def benchmark_run(label: str, seed: int) -> dict[str, object] | None:
    letters = ", ".join(SINGLE_LETTER_QUERIES)
    print(f"\n{label} seed: {seed}")
    print(f"Single-letter sample: {letters}")
    print("entries  multiword ms/search  single-letter ms/search  repetitions")
    snapshot = None
    for size in SIZES:
        history = build_history(size, seed)
        with RustSearchDaemon(history) as native_candidates:
            repetitions = repetitions_for(size)
            multiword_ms, _ = benchmark(native_candidates, QUERIES, repetitions)
            single_letter_ms, _ = benchmark(
                native_candidates,
                SINGLE_LETTER_QUERIES,
                repetitions,
            )
            print(
                f"{size:>7,}  {multiword_ms:>19.2f}  "
                f"{single_letter_ms:>23.2f}  {repetitions:>11}"
            )
            if size == SNAPSHOT_SIZE:
                snapshot = snapshot_results(size, native_candidates, seed)
    return snapshot


def default_custom_history_path() -> Path:
    state_home = os.environ.get("XDG_STATE_HOME", "").strip()
    if state_home:
        return Path(state_home) / "zsh-flex-history" / "history.db"
    home = Path.home()
    if sys.platform == "darwin":
        return home / "Library" / "Application Support" / "zsh-flex-history" / "history.db"
    return home / ".local" / "state" / "zsh-flex-history" / "history.db"


def normalize_cwd_value(cwd: str) -> str:
    return cwd.strip()


def load_real_history_read_only(path: Path) -> list[HistoryEntry]:
    """Copy history rows through a read-only connection into benchmark storage."""
    if not path.is_file():
        return []

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
            HistoryEntry(
                cleaned,
                normalize_cwd_value(cwd) if isinstance(cwd, str) else "",
                timestamp if isinstance(timestamp, str) else "",
                bool(failed),
            )
        )
    return entries


def benchmark_real_history() -> None:
    path = default_custom_history_path()
    history = load_real_history_read_only(path)
    if not history:
        print(f"\nReal custom-history benchmark skipped: no readable entries at {path}")
        return

    with RustSearchDaemon(history) as native_candidates:
        repetitions = repetitions_for(len(history))
        multiword_ms, _ = benchmark(native_candidates, QUERIES, repetitions)
        single_letter_ms, _ = benchmark(
            native_candidates,
            SINGLE_LETTER_QUERIES,
            repetitions,
        )
    print("\nReal custom-history (copied read-only; Rust daemon search)")
    print(f"database: {path}")
    print(f"entries: {len(history):,}; repetitions: {repetitions}")
    print(f"multiword ms/search: {multiword_ms:.2f}")
    print(f"single-letter ms/search ({', '.join(SINGLE_LETTER_QUERIES)}): {single_letter_ms:.2f}")


def main() -> int:
    ensure_rust_binary()
    snapshot = benchmark_run("Fixed", FIXED_SEED)
    if snapshot is None:
        raise RuntimeError(f"SNAPSHOT_SIZE {SNAPSHOT_SIZE} is not present in SIZES")
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
