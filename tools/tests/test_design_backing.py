import unittest

from . import _support  # noqa: F401
from rusekit.gov import design_backing as db  # noqa: E402


def feat(parity, design):
    return {"trace": {"parity": parity, "design": design}}


class TestEvaluate(unittest.TestCase):
    def test_circular_design_is_flagged(self):
        r = db.evaluate({"F-1": feat(["VIM-MODE-1"], ["../docs/parity/terminal.md"])})
        self.assertEqual(r["circular"], [("F-1", "../docs/parity/terminal.md")])

    def test_broken_link_is_flagged(self):
        r = db.evaluate({"F-1": feat(["VIM-MODE-1"], ["../docs/design/does-not-exist.md"])})
        self.assertEqual(r["broken"], [("F-1", "../docs/design/does-not-exist.md")])

    def test_real_existing_design_passes(self):
        # input-engine.md exists in the repo.
        r = db.evaluate({"F-1": feat(["VIM-MODE-1"], ["../docs/design/input-engine.md"])})
        self.assertEqual(r["circular"], [])
        self.assertEqual(r["broken"], [])
        self.assertEqual(r["breadth_only"], [])

    def test_parity_without_any_real_design_is_breadth_only(self):
        # Only a circular ref → no real backing → breadth-only (but circular is also flagged).
        r = db.evaluate({"F-1": feat(["VIM-MODE-1"], ["../docs/parity/vim.md"])})
        self.assertEqual(r["breadth_only"], ["F-1"])

    def test_no_parity_is_never_breadth_only(self):
        r = db.evaluate({"F-1": feat([], [])})
        self.assertEqual(r["breadth_only"], [])

    def test_anchor_is_stripped_when_resolving(self):
        # architecture.md#6 must resolve to architecture.md (existing), not fail on the anchor.
        r = db.evaluate({"F-1": feat(["X-1"], ["../docs/architecture/architecture.md#6"])})
        self.assertEqual(r["broken"], [])


if __name__ == "__main__":
    unittest.main()
