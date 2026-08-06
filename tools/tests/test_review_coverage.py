import unittest

from . import _support  # noqa: F401
from rusekit.gov import review_coverage as rc  # noqa: E402


def axes(*rows):
    # rows: (id, domain, method, automated_by)
    return [{"id": i, "domain": d, "method": m, "automated_by": a} for (i, d, m, a) in rows]


class TestEvaluate(unittest.TestCase):
    CHECKERS = {"dependency-check", "design_code", "spec-validate"}

    def test_valid_link_is_counted_not_broken(self):
        r = rc.evaluate(axes(("RA-ARCH-008", "ARCH", "machine", "dependency-check")), self.CHECKERS)
        self.assertEqual(r["broken"], [])
        self.assertEqual(r["automated"], 1)
        self.assertEqual(r["machine_wired"], 1)
        self.assertEqual(r["gaps"], [])

    def test_broken_link_is_flagged(self):
        r = rc.evaluate(axes(("RA-X-1", "X", "mixed", "no_such_checker")), self.CHECKERS)
        self.assertEqual(r["broken"], [("RA-X-1", "no_such_checker")])

    def test_machine_without_automation_is_a_gap(self):
        r = rc.evaluate(axes(("RA-X-2", "X", "machine", None)), self.CHECKERS)
        self.assertEqual(r["gaps"], ["RA-X-2"])
        self.assertEqual(r["machine_wired"], 0)

    def test_llm_axis_without_automation_is_not_a_gap(self):
        r = rc.evaluate(axes(("RA-X-3", "X", "llm", None)), self.CHECKERS)
        self.assertEqual(r["gaps"], [])

    def test_load_axes_applies_domain_default_method(self):
        doc = {"domains": [{"id": "ARCH", "default_method": "llm",
                            "axes": [{"id": "RA-ARCH-004", "title": "x"},
                                     {"id": "RA-ARCH-008", "title": "y", "method": "machine"}]}]}
        loaded = {a["id"]: a["method"] for a in rc.load_axes(doc)}
        self.assertEqual(loaded["RA-ARCH-004"], "llm")   # inherited
        self.assertEqual(loaded["RA-ARCH-008"], "machine")  # explicit


class TestRepo(unittest.TestCase):
    def test_repo_has_no_broken_links(self):
        # The live rubric must never point an automated_by at a missing checker (that would fail gov check).
        self.assertEqual(rc.main([]), 0)

    def test_the_four_arch_links_resolve(self):
        checkers = rc.valid_checkers()
        for name in ("dependency-check", "spec-validate", "design_code"):
            self.assertIn(name, checkers, f"{name} must be a known checker for the ARCH links to resolve")


if __name__ == "__main__":
    unittest.main()
