"""Equivalence checks for the optional native flex matcher."""

from __future__ import annotations

import random
import unittest

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
        self.assertEqual(actual.positions, expected.positions, (query, candidate))

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
                [(item.score, item.positions, item.text) for item in actual_results],
                [(item.score, item.positions, item.text) for item in expected_results],
                query,
            )


if __name__ == "__main__":
    unittest.main()
