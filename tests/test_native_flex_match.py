"""Equivalence checks for the required native flex matcher."""

from __future__ import annotations

import random
import socket
import tempfile
import threading
import unittest
from array import array
from pathlib import Path
from unittest.mock import patch

from zsh_flex_history import _flex_match as native
from zsh_flex_history import engine


def python_flex_match(
    query: str,
    candidate: str,
    *,
    candidate_lower: str | None = None,
) -> engine.MatchResult | None:
    """Independent test reference for the native scoring contract."""
    q = "".join(character for character in query.lower() if not character.isspace())
    c = candidate_lower if candidate_lower is not None else candidate.lower()
    if not q:
        return engine.MatchResult(candidate, 0, text_lower=c)

    at = 0
    first = previous = contiguous = gap_penalty = boundary_bonus = 0
    for index, character in enumerate(q):
        position = c.find(character, at)
        if position == -1:
            return None
        if index == 0:
            first = position
            if position == 0:
                boundary_bonus += 12
            elif candidate[position - 1] in " _-/.:":
                boundary_bonus += 8
        else:
            gap = position - previous - 1
            gap_penalty += gap * 2
            if gap == 0:
                contiguous += 10
            if candidate[position - 1] in " _-/.:":
                boundary_bonus += 6
        previous = position
        at = position + 1

    span = previous - first + 1
    score = contiguous + boundary_bonus + max(0, 30 - first)
    score += max(0, 20 - (span - len(q)))
    score -= gap_penalty + len(candidate) // 8
    return engine.MatchResult(candidate, score, text_lower=c)


class NativeFlexMatchTests(unittest.TestCase):
    def assert_matches_python(self, query: str, candidate: str) -> None:
        expected = python_flex_match(query, candidate)
        actual = engine.flex_match(query, candidate)
        if expected is None:
            self.assertIsNone(actual, (query, candidate))
            return
        self.assertIsNotNone(actual, (query, candidate))
        assert actual is not None
        self.assertEqual(actual.score, expected.score, (query, candidate))

    def test_representative_queries(self) -> None:
        for query, candidate in (
            ("git st", "git status --short"),
            ("gst", "git status --short"),
            ("pyth", "python -m pytest"),
            ("docker", "docker compose up"),
            ("", "any command"),
            ("   ", "any command"),
            ("écl", "Éclair command"),
            ("不存在", "a command"),
        ):
            self.assert_matches_python(query, candidate)

    def test_randomized_queries(self) -> None:
        rng = random.Random(20260817)
        alphabet = "abcdeABCDE012 _-/.:\né"
        for _ in range(10_000):
            query = "".join(rng.choice(alphabet) for _ in range(rng.randrange(10)))
            candidate = "".join(rng.choice(alphabet) for _ in range(rng.randrange(1, 80)))
            self.assert_matches_python(query, candidate)

    def test_batch_history_scan_matches_python(self) -> None:
        history = [
            engine.make_history_entry(command)
            for command in (
                "git status --short",
                "git switch main",
                "python -m pytest -q",
                "docker compose up --build",
                "éclair deploy production",
            )
        ]
        native_candidates = engine.build_native_history_candidates(history)
        self.assertIsNotNone(native_candidates)
        self.assertEqual(len(native_candidates), len(history))
        for query, candidate_indices in (
            ("git st", None),
            ("gst", None),
            ("py", [1, 2, 4]),
            ("écl", [0, 4]),
            ("does-not-exist", None),
        ):
            expected_results, expected_indices = engine.search_history_only(
                query,
                history,
                candidate_indices=candidate_indices,
            )
            actual_results, actual_indices = engine.search_history_only(
                query,
                history,
                candidate_indices=candidate_indices,
                native_candidates=native_candidates,
            )
            self.assertEqual(actual_indices, expected_indices, query)
            self.assertEqual(
                [(item.score, item.text) for item in actual_results],
                [(item.score, item.text) for item in expected_results],
                query,
            )

    def test_full_ordered_batch_search_matches_scalar_native_pipeline(self) -> None:
        history = [
            engine.make_history_entry("git status --short", cwd="/repo"),
            engine.make_history_entry("git switch main", cwd="/other"),
            engine.make_history_entry("git stash list", cwd="/repo", failed=True),
            engine.make_history_entry("python -m pytest -q", cwd="/repo"),
            engine.make_history_entry("git status --short", cwd="/other"),
            engine.make_history_entry("docker compose up --build", cwd="/other"),
            engine.make_history_entry("éclair deploy production", cwd="/repo"),
        ]
        native_candidates = engine.build_native_history_candidates(history)
        self.assertIsNotNone(native_candidates)

        for query, candidate_indices in (
            ("", None),
            (" ", None),
            ("g", None),
            ("git st", None),
            ("gst", None),
            ("py", [1, 2, 3, 5, 6]),
            ("écl", [0, 4, 6]),
            ("does-not-exist", None),
        ):
            with patch.object(engine, "flex_match", python_flex_match):
                expected_results, expected_indices = engine.search(
                    query,
                    history,
                    candidate_indices=candidate_indices,
                    limit=4,
                    cwd=Path("/repo"),
                )
            actual_results, actual_indices = engine.search(
                query,
                history,
                candidate_indices=candidate_indices,
                limit=4,
                cwd=Path("/repo"),
                native_candidates=native_candidates,
            )
            self.assertEqual(actual_indices, expected_indices, query)
            self.assertEqual(actual_results, expected_results, query)

    def test_native_ranked_daemon_search_matches_python_pipeline(self) -> None:
        history = [
            engine.make_history_entry("git status --short", cwd="/repo"),
            engine.make_history_entry("git switch main", cwd="/other"),
            engine.make_history_entry("git stash list", cwd="/repo", failed=True),
            engine.make_history_entry("git status --short", cwd="/other"),
            engine.make_history_entry("python -m pytest -q", cwd="/repo"),
            engine.make_history_entry("printf '%s' unmatched", cwd="/other"),
            engine.make_history_entry("éclair deploy production", cwd="/repo"),
        ]
        native_candidates = engine.build_native_history_candidates(history)
        self.assertIsNotNone(native_candidates)

        for query, candidate_indices in (
            ("", None),
            ("g", None),
            ("git st", None),
            ("gst", None),
            ("git status --short", None),
            ("'git st", None),
            ("py", [1, 2, 4, 5, 6]),
            ("écl", [0, 3, 6]),
            ("does-not-exist", None),
        ):
            with patch.object(engine, "flex_match", python_flex_match):
                expected_results, expected_indices = engine.search(
                    query,
                    history,
                    candidate_indices=candidate_indices,
                    limit=4,
                    cwd=Path("/repo"),
                )
            actual = engine.search_history_ranked_native(
                query,
                history,
                native_candidates,
                candidate_indices=candidate_indices,
                limit=4,
                current_cwd="/repo",
            )
            actual_results, actual_indices, actual_count = actual
            expected_payload = (
                expected_indices
                if query and len(expected_indices) <= engine.MAX_CACHED_CANDIDATE_INDICES
                else None
            )
            self.assertEqual(actual_indices, expected_payload, query)
            self.assertEqual(actual_count, len(expected_indices), query)
            self.assertEqual(actual_results, expected_results, query)
            self.assertEqual(
                engine.apply_prefix_priority(
                    query,
                    actual_results,
                    limit=4,
                    current_cwd="/repo",
                ),
                actual_results,
                query,
            )

            frame = engine.search_history_response_frame_native(
                query,
                history,
                native_candidates,
                candidate_indices=candidate_indices,
                limit=4,
                current_cwd="/repo",
            )
            self.assertEqual(frame[:4], b"ZFH\x01")
            self.assertEqual(int.from_bytes(frame[4:8], "little"), len(frame) - 8)
            parsed_response = native.parse_search_response(frame)
            self.assertIsNotNone(parsed_response)
            assert parsed_response is not None
            parsed_results, parsed_indices, parsed_count = parsed_response
            self.assertEqual(parsed_indices, expected_payload, query)
            self.assertEqual(parsed_count, len(expected_indices), query)
            self.assertEqual(
                parsed_results,
                [
                    (
                        item.text,
                        item.score,
                        item.exact,
                        item.recency,
                        item.cwd,
                        item.failed,
                        list(item.words),
                    )
                    for item in expected_results
                ],
                query,
            )

        self.assertIsNone(native.parse_search_response(b"not a frame"))

    def test_native_search_request_serialization(self) -> None:
        serialized = native.serialize_search_request(
            "git st",
            array("I", [1, 3, 8]),
            100,
            "/repo",
        )
        self.assertEqual(serialized[:4], b"ZFH\x01")
        self.assertEqual(int.from_bytes(serialized[4:8], "little"), len(serialized) - 8)
        self.assertEqual(serialized[8], 2)
        self.assertNotIn(b"search_history", serialized)
        minimal = native.serialize_search_request("x")
        self.assertEqual(minimal[:4], b"ZFH\x01")
        self.assertLess(len(minimal), len(serialized))

    def test_native_daemon_round_trip(self) -> None:
        history = [
            engine.HistoryEntry(
                "git status --short",
                cwd="/repo",
                text_lower="git status --short",
                words=("git", "status", "--short"),
            )
        ]
        candidates = engine.build_native_history_candidates(history)
        received: list[tuple[str, list[int] | None, int | None, str | None]] = []
        with tempfile.TemporaryDirectory() as directory:
            socket_path = Path(directory) / "history.sock"
            native_server = engine._NativeDaemonServer(str(socket_path))

            def serve_twice() -> None:
                for _ in range(2):
                    request = native_server.accept_search()
                    received.append(
                        (request.query, request.candidate_indices, request.limit, request.cwd)
                    )
                    frame = engine.native_history_response_frame(
                        request.query,
                        candidates,
                        candidate_indices=request.candidate_indices,
                        limit=request.limit,
                        current_cwd=request.cwd,
                    )
                    self.assertTrue(request.respond_frame(frame))

            server = threading.Thread(target=serve_twice)
            server.start()
            exchanged, parsed = engine._native_search_daemon(
                str(socket_path), "git st", array("I", [0]), 100, "/repo"
            )
            client = engine.HistoryDaemonClient(
                socket_path,
                Path(directory) / "unused-history",
                Path(directory) / "unused-script",
            )
            client_result = client.search_history(
                "git st", candidate_indices=array("I", [0]), limit=100, cwd="/repo"
            )
            server.join(timeout=2)

        self.assertFalse(server.is_alive())
        self.assertTrue(exchanged)
        self.assertEqual(
            received,
            [("git st", [0], 100, "/repo"), ("git st", [0], 100, "/repo")],
        )
        self.assertIsNotNone(parsed)
        assert parsed is not None
        self.assertEqual(parsed[1:], ([0], 1))
        self.assertEqual(parsed[0][0][0], "git status --short")
        self.assertIsNotNone(client_result)
        assert client_result is not None
        client_results, client_indices, client_count = client_result
        self.assertEqual(client_indices, [0])
        self.assertEqual(client_count, 1)
        self.assertEqual(client_results[0].text, "git status --short")

    def test_native_daemon_server_dispatch(self) -> None:
        candidates = engine.build_native_history_candidates([])
        captured: dict[str, object] = {}
        with tempfile.TemporaryDirectory() as directory:
            socket_path = Path(directory) / "native-server.sock"
            server = engine._NativeDaemonServer(str(socket_path))

            def serve_search() -> None:
                request = server.accept_search()
                captured["query"] = request.query
                captured["candidate_indices"] = request.candidate_indices
                captured["limit"] = request.limit
                captured["cwd"] = request.cwd
                response = engine.native_history_response_frame(
                    request.query,
                    candidates,
                    candidate_indices=request.candidate_indices,
                    limit=request.limit,
                    current_cwd=request.cwd,
                )
                captured["responded"] = request.respond_frame(response)

            server_thread = threading.Thread(target=serve_search)
            server_thread.start()

            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as malformed_client:
                malformed_client.connect(str(socket_path))
                malformed_client.sendall(b"notframe")
                malformed_response = malformed_client.recv(4096)

            ping = native.ping_daemon(str(socket_path))
            exchanged, search_response = engine._native_search_daemon(
                str(socket_path), "git st", [3], 100, "/repo"
            )
            server_thread.join(timeout=2)
            socket_mode = socket_path.stat().st_mode & 0o777

        self.assertFalse(server_thread.is_alive())
        self.assertEqual(malformed_response[:4], b"ZFH\x01")
        self.assertEqual(malformed_response[8], 0xFF)
        self.assertTrue(ping)
        self.assertTrue(exchanged)
        self.assertEqual(search_response, ([], [], 0))
        self.assertEqual(socket_mode, 0o600)
        self.assertEqual(
            captured,
            {
                "query": "git st",
                "candidate_indices": [3],
                "limit": 100,
                "cwd": "/repo",
                "responded": True,
            },
        )


if __name__ == "__main__":
    unittest.main()
