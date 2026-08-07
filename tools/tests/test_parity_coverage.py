import unittest

from . import _support  # noqa: F401
from rusekit.gov import parity_coverage as pc  # noqa: E402


def row(pid, target, compat):
    return {"id": pid, "target": target, "compat": compat, "doc": "vim.md"}


class TestTargeted(unittest.TestCase):
    def test_l1_l2_without_excluded_compat_are_targeted(self):
        rows = [row("VIM-MODE-7", "L2", None), row("VIM-OP-1", "L1", None)]
        self.assertEqual(pc.targeted_ids(rows), {"VIM-MODE-7", "VIM-OP-1"})

    def test_l3_is_not_a_target(self):
        self.assertEqual(pc.targeted_ids([row("VIM-SCRIPT-1", "L3", None)]), set())

    def test_unsupported_and_intentionally_different_auto_excluded(self):
        rows = [row("VIM-X-1", "L2", "Unsupported"), row("VIM-X-2", "L2", "Intentionally-different")]
        self.assertEqual(pc.targeted_ids(rows), set())

    def test_missing_target_is_not_counted(self):
        # A malformed row with an ID but no L-cell is not treated as a target (avoids false orphans).
        self.assertEqual(pc.targeted_ids([row("VIM-Y-1", None, None)]), set())


class TestCollectParity(unittest.TestCase):
    def test_walks_nested_trace_parity(self):
        doc = {"features": [{"id": "F-003", "trace": {"parity": ["VIM-MODE-1", "VIM-OP-1"]}},
                            {"id": "F-009", "trace": {"parity": ["VIM-SEARCH-1"]}}]}
        acc = set()
        pc._collect_parity(doc, acc)
        self.assertEqual(acc, {"VIM-MODE-1", "VIM-OP-1", "VIM-SEARCH-1"})

    def test_ignores_non_parity_keys(self):
        acc = set()
        pc._collect_parity({"design": ["docs/x.md"], "parity": ["VIM-A-1"]}, acc)
        self.assertEqual(acc, {"VIM-A-1"})


class TestIdRegex(unittest.TestCase):
    def test_accepts_known_prefixes_and_shapes(self):
        for ok in ("VIM-MODE-7", "COM-5", "VIM-EX-GLOBAL", "NVIM-UI-3", "REM-SERVICE-1"):
            self.assertTrue(pc.ID_RE.match(ok), ok)

    def test_rejects_non_ids(self):
        for bad in ("Mode", "L2", "high", "the-quick-brown"):
            self.assertFalse(pc.ID_RE.match(bad), bad)


if __name__ == "__main__":
    unittest.main()
