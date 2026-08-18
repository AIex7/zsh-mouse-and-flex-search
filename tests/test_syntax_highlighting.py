"""Native syntax-highlighting tests."""

from __future__ import annotations

import random
import unittest
from unittest.mock import patch

from zsh_flex_history import syntax_highlighting


def token_names(tokens: list[str] | bytes) -> list[str]:
    if isinstance(tokens, bytes):
        return [syntax_highlighting.TOKEN_NAMES[token] for token in tokens]
    return tokens


def editing_sequence(command: str) -> list[str]:
    states = [command[:index] for index in range(len(command) + 1)]
    states.extend(command[:index] for index in range(len(command) - 1, max(0, len(command) - 8), -1))
    states.append(command)
    midpoint = len(command) // 2
    states.append(command[:midpoint] + "X" + command[midpoint:])
    states.append(command)
    states.append(command)
    return states


class SyntaxHighlightingTests(unittest.TestCase):
    def test_compact_native_token_styles_match_named_styles(self) -> None:
        for token_id, token_name in enumerate(syntax_highlighting.TOKEN_NAMES):
            self.assertEqual(
                syntax_highlighting.ansi_for_token(token_id),
                syntax_highlighting.ansi_for_token(token_name),
            )

    def test_randomized_incremental_edits_match_fresh_native_lexing(self) -> None:
        def deterministic_command_state(word: str, word_complete: bool) -> str:
            if word.startswith(("a", "g")):
                return "valid"
            return "error" if word_complete else "pending"

        rng = random.Random(20_260_818)
        alphabet = "abcgitXYZ09 _-=/$#\\'\"`(){}[];&|<>é"
        query = ""
        incremental = syntax_highlighting.IncrementalHighlighter()
        with patch.object(
            syntax_highlighting,
            "_command_state",
            deterministic_command_state,
        ):
            for _ in range(2_000):
                operation = rng.randrange(3)
                if operation == 0 or not query:
                    position = rng.randrange(len(query) + 1)
                    query = query[:position] + rng.choice(alphabet) + query[position:]
                elif operation == 1:
                    position = rng.randrange(len(query))
                    query = query[:position] + query[position + 1 :]
                else:
                    position = rng.randrange(len(query))
                    query = query[:position] + rng.choice(alphabet) + query[position + 1 :]
                self.assertEqual(
                    token_names(incremental.highlight(query)),
                    token_names(syntax_highlighting.highlight_tokens(query)),
                    query,
                )

    def test_native_incremental_matches_fresh_native_lexing(self) -> None:
        commands = (
            "git status --short",
            "FOO=bar python -m pytest -q",
            "if [[ -n $HOME ]]; then printf '%s\\n' \"$HOME\"; fi",
            "echo $(git branch --show-current) | rg 'feature|fix' # current work",
            "value=$((1 + (2 * 3))) && deploy production --tag=éclair",
            "python -m worker.dispatch --payload '{\"key\": \"value\"}'",
        )
        incremental = syntax_highlighting.IncrementalHighlighter()
        for command in commands:
            for query in editing_sequence(command):
                self.assertEqual(
                    token_names(incremental.highlight(query)),
                    token_names(syntax_highlighting.highlight_tokens(query)),
                    query,
                )

    def test_native_complete_highlight_returns_one_token_per_character(self) -> None:
        for query in (
            "",
            "git",
            "unknown-command --flag",
            "echo ${value:-fallback}",
            "printf '%s' unterminated'",
            "command one && command two || command three",
            "écho café",
        ):
            tokens = syntax_highlighting.highlight_tokens(query)
            self.assertIsInstance(tokens, bytes)
            self.assertEqual(len(tokens), len(query))
            self.assertTrue(all(token < len(syntax_highlighting.TOKEN_NAMES) for token in tokens))


if __name__ == "__main__":
    unittest.main()
