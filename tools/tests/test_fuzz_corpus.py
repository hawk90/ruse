"""Every cargo-fuzz target has a committed seed corpus and is in the weekly matrix (#1319).

The PR `fuzz-smoke` job (ci.yml) runs each target from `fuzz/corpus/<target>/`, and the weekly fuzz.yml
starts from the same directory; a target without seeds starts from nothing, and one missing from the matrix
never gets its long run.
"""
import os
import tomllib
import unittest

import yaml

from . import _support  # noqa: F401

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
FUZZ = os.path.join(ROOT, "fuzz")


def _targets() -> list[str]:
    with open(os.path.join(FUZZ, "Cargo.toml"), "rb") as fh:
        return [b["name"] for b in tomllib.load(fh).get("bin", [])]


class TestFuzzCorpus(unittest.TestCase):
    def test_every_target_has_seeds(self):
        targets = _targets()
        self.assertTrue(targets, "fuzz/Cargo.toml declares no [[bin]] target")
        for t in targets:
            with self.subTest(target=t):
                d = os.path.join(FUZZ, "corpus", t)
                seeds = os.listdir(d) if os.path.isdir(d) else []
                self.assertTrue(seeds, f"fuzz/corpus/{t}/ has no seed")

    def test_every_corpus_belongs_to_a_target(self):
        d = os.path.join(FUZZ, "corpus")
        stale = sorted(set(os.listdir(d)) - set(_targets())) if os.path.isdir(d) else []
        self.assertEqual(stale, [], "fuzz/corpus/ holds a directory for no fuzz target")

    def test_the_weekly_matrix_runs_every_target(self):
        with open(os.path.join(ROOT, ".github", "workflows", "fuzz.yml"), encoding="utf-8") as fh:
            matrix = yaml.safe_load(fh)["jobs"]["fuzz"]["strategy"]["matrix"]["target"]
        self.assertEqual(sorted(matrix), sorted(_targets()))


if __name__ == "__main__":
    unittest.main()
