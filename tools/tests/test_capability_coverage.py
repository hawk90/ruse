import unittest

from . import _support  # noqa: F401
from rusekit.gov import capability_coverage as cc  # noqa: E402


class TestEvaluate(unittest.TestCase):
    FEATURES = {"F-009", "F-014"}

    def test_missing_prd_field_is_undeclared(self):
        r = cc.evaluate({"CAP-X": {"title": "x"}}, self.FEATURES)
        self.assertEqual(r["undeclared"], ["CAP-X"])

    def test_real_link_counts_as_linked(self):
        r = cc.evaluate({"CAP-X": {"prd": ["F-009"]}}, self.FEATURES)
        self.assertEqual(r["linked"], 1)
        self.assertEqual(r["undeclared"], [])
        self.assertEqual(r["broken"], [])

    def test_explicit_empty_is_declared_not_undeclared(self):
        r = cc.evaluate({"CAP-X": {"prd": []}}, self.FEATURES)
        self.assertEqual(r["declared_empty"], 1)
        self.assertEqual(r["undeclared"], [])

    def test_broken_link_is_flagged(self):
        r = cc.evaluate({"CAP-X": {"prd": ["F-999"]}}, self.FEATURES)
        self.assertEqual(r["broken"], [("CAP-X", "F-999")])

    def test_real_caps_file_is_fully_declared(self):
        # The live capabilities.yaml must have no undeclared / broken entries (the gate is green in-tree).
        r = cc.evaluate(cc.load_caps(), cc.prd_feature_ids())
        self.assertEqual(r["undeclared"], [])
        self.assertEqual(r["broken"], [])


if __name__ == "__main__":
    unittest.main()
