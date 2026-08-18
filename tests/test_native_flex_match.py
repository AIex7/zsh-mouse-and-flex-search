"""Equivalence checks for the optional native flex matcher."""

from __future__ import annotations

import json
import random
import socket
import tempfile
import threading
import unittest
from array import array
from pathlib import Path
from unittest.mock import patch

from zsh_flex_history import engine


class NativeFlexMatchTests(unittest.TestCase):
    def setUp(self) -> None:
        if engine._native_flex_match is None:
            self.skipTest("native flex extension has not been built")

    def assert_matches_python(self, query: str, candidate: str) -> None:
        expected = engine._python_flex_match(query, candidate)
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

    def test_full_ordered_search_matches_python_fallback(self) -> None:
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
            with patch.object(engine, "_native_flex_match", None):
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
            with patch.object(engine, "_native_flex_match", None):
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
            self.assertIsNotNone(actual)
            assert actual is not None
            actual_results, actual_indices, actual_count = actual
            expected_payload = engine.daemon_matched_indices_payload(query, expected_indices)
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

            serialized = engine.search_history_response_json_native(
                query,
                history,
                native_candidates,
                candidate_indices=candidate_indices,
                limit=4,
                current_cwd="/repo",
            )
            self.assertIsNotNone(serialized)
            assert serialized is not None
            self.assertEqual(
                json.loads(serialized),
                {
                    "ok": True,
                    "history_results": [
                        engine.match_result_to_payload(item) for item in expected_results
                    ],
                    "matched_indices": expected_payload,
                    "matched_indices_omitted": expected_payload is None,
                    "matched_count": len(expected_indices),
                },
                query,
            )
            self.assertIsNotNone(engine._native_parse_search_response)
            parsed_response = engine._native_parse_search_response(serialized.encode("utf-8"))
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

        self.assertIsNone(engine._native_parse_search_response(b"not json"))
        self.assertIsNone(engine._native_parse_search_response(b'{"ok":false}'))

    def test_native_search_request_serialization(self) -> None:
        self.assertIsNotNone(engine._native_serialize_search_request)
        serialized = engine._native_serialize_search_request(
            "git st",
            array("I", [1, 3, 8]),
            100,
            "/repo",
        )
        self.assertTrue(serialized.endswith(b"\n"))
        self.assertEqual(
            json.loads(serialized),
            {
                "action": "search_history",
                "query": "git st",
                "candidate_indices": [1, 3, 8],
                "limit": 100,
                "cwd": "/repo",
            },
        )
        self.assertEqual(
            json.loads(engine._native_serialize_search_request("x")),
            {"action": "search_history", "query": "x"},
        )

    def test_native_daemon_round_trip(self) -> None:
        self.assertIsNotNone(engine._native_search_daemon)
        response = {
            "ok": True,
            "history_results": [
                {
                    "text": "git status --short",
                    "score": 72,
                    "exact": False,
                    "recency": -3,
                    "cwd": "/repo",
                    "failed": False,
                    "words": ["git", "status", "--short"],
                }
            ],
            "matched_indices": [3, 8],
            "matched_indices_omitted": False,
            "matched_count": 2,
        }
        received: list[bytes] = []
        with tempfile.TemporaryDirectory() as directory:
            socket_path = str(Path(directory) / "history.sock")
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as listener:
                listener.bind(socket_path)
                listener.listen(1)

                def serve_twice() -> None:
                    for _ in range(2):
                        connection, _ = listener.accept()
                        with connection:
                            request = bytearray()
                            while b"\n" not in request:
                                chunk = connection.recv(65_536)
                                if not chunk:
                                    break
                                request.extend(chunk)
                            received.append(bytes(request))
                            connection.sendall(json.dumps(response).encode("utf-8") + b"\n")

                server = threading.Thread(target=serve_twice)
                server.start()
                exchanged, parsed = engine._native_search_daemon(
                    socket_path,
                    "git st",
                    array("I", [3, 8]),
                    100,
                    "/repo",
                )
                client = engine.HistoryDaemonClient(
                    Path(socket_path),
                    Path(directory) / "unused-history",
                    Path(directory) / "unused-script",
                )
                client_result = client.search_history(
                    "git st",
                    candidate_indices=array("I", [3, 8]),
                    limit=100,
                    cwd="/repo",
                )
                server.join(timeout=2)

        self.assertFalse(server.is_alive())
        self.assertTrue(exchanged)
        self.assertEqual(len(received), 2)
        self.assertEqual(
            json.loads(received[0]),
            {
                "action": "search_history",
                "query": "git st",
                "candidate_indices": [3, 8],
                "limit": 100,
                "cwd": "/repo",
            },
        )
        self.assertEqual(json.loads(received[1]), json.loads(received[0]))
        self.assertEqual(
            parsed,
            (
                [
                    (
                        "git status --short",
                        72,
                        False,
                        -3,
                        "/repo",
                        False,
                        ["git", "status", "--short"],
                    )
                ],
                [3, 8],
                2,
            ),
        )
        self.assertIsNotNone(client_result)
        assert client_result is not None
        client_results, client_indices, client_count = client_result
        self.assertEqual(client_indices, [3, 8])
        self.assertEqual(client_count, 2)
        self.assertEqual(
            client_results,
            [
                engine.MatchResult(
                    text="git status --short",
                    score=72,
                    exact=False,
                    recency=-3,
                    cwd="/repo",
                    failed=False,
                    words=("git", "status", "--short"),
                )
            ],
        )

    def test_native_daemon_server_dispatch(self) -> None:
        self.assertIsNotNone(engine._NativeDaemonServer)
        response = {
            "ok": True,
            "history_results": [],
            "matched_indices": [3],
            "matched_indices_omitted": False,
            "matched_count": 1,
        }
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
                captured["responded"] = request.respond_serialized(
                    json.dumps(response, separators=(",", ":"))
                )

            server_thread = threading.Thread(target=serve_search)
            server_thread.start()

            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as malformed_client:
                malformed_client.connect(str(socket_path))
                malformed_client.sendall(b"not json\n")
                malformed_response = malformed_client.recv(4096)

            ping = engine.daemon_send_request(socket_path, {"action": "ping"})
            unknown = engine.daemon_send_request(socket_path, {"action": "unknown"})
            search_response = engine.daemon_send_request(
                socket_path,
                {
                    "action": "search_history",
                    "query": "git st",
                    "candidate_indices": [-1, 3],
                    "limit": 100,
                    "cwd": "/repo",
                },
            )
            server_thread.join(timeout=2)
            socket_mode = socket_path.stat().st_mode & 0o777

        self.assertFalse(server_thread.is_alive())
        self.assertEqual(
            json.loads(malformed_response),
            {"ok": False, "error": "invalid request"},
        )
        self.assertEqual(ping, {"ok": True})
        self.assertEqual(unknown, {"ok": False, "error": "unknown action"})
        self.assertEqual(search_response, response)
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
