import unittest

from . import _support  # noqa: F401
from rusekit import contract, model as model_mod  # noqa: E402


class TestContract(unittest.TestCase):
    def test_kind_names_and_risk_ladder(self):
        kinds = contract.kind_names()
        for k in ("docs-editorial", "docs-semantic", "implementation", "spec",
                  "architecture", "contract"):
            self.assertIn(k, kinds)
        self.assertEqual(contract.kind_risk("docs-editorial"), 0)
        self.assertEqual(contract.kind_risk("architecture"), 3)
        self.assertGreater(contract.kind_risk("contract"),
                           contract.kind_risk("docs-semantic"))

    def test_validate_flags_unknown_affected_id(self):
        m = model_mod.load()
        c = {"kind": "implementation", "goal": "x",
             "affected": {"requirements": ["C-DOES-NOT-EXIST"]}}
        errors, _warns = contract.validate(c, m)
        self.assertTrue(any("C-DOES-NOT-EXIST" in e for e in errors))

    def test_validate_accepts_real_ids(self):
        m = model_mod.load()
        c = {"kind": "implementation", "goal": "x",
             "affected": {"requirements": ["C-TRANSACTION"],
                          "capabilities": ["CAP-EDIT-CORE"]}}
        errors, _warns = contract.validate(c, m)
        self.assertEqual(errors, [])

    def test_affected_ids_collects_all_categories(self):
        c = {"affected": {"capabilities": ["CAP-EDIT-CORE"],
                          "requirements": ["C-TRANSACTION"],
                          "invariants": ["INV-TXN"]}}
        ids = contract.affected_ids(c)
        self.assertEqual(set(ids), {"CAP-EDIT-CORE", "C-TRANSACTION", "INV-TXN"})


if __name__ == "__main__":
    unittest.main()
