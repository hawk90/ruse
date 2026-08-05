import os
import unittest

from . import _support  # noqa: F401  (sys.path side effect)
from rusekit import repo  # noqa: E402
from change.classify import classify_changeset  # noqa: E402


class TestClassify(unittest.TestCase):
    def test_declared_below_observed_fails(self):
        # touching the architecture spec forces >= architecture; docs-editorial is too low.
        cl = classify_changeset(["spec/ARCHITECTURE.md"], "docs-editorial")
        self.assertEqual(cl.observed_kind, "architecture")
        self.assertFalse(cl.ok)

    def test_implementation_on_rust_passes(self):
        cl = classify_changeset(["crates/core/src/lib.rs"], "implementation")
        self.assertEqual(cl.observed_kind, "implementation")
        self.assertTrue(cl.ok)

    def test_protocol_crate_forces_contract(self):
        # implementation (risk 2) is below the plugin-protocol floor (contract, risk 3).
        cl = classify_changeset(["crates/plugin-protocol/src/x.rs"], "implementation")
        self.assertEqual(cl.observed_kind, "contract")
        self.assertFalse(cl.ok)

    def test_docs_prose_is_human_judgment_not_a_floor(self):
        cl = classify_changeset(["docs/design/document-model.md"], "docs-editorial")
        self.assertTrue(cl.ok)                 # no forced floor
        self.assertTrue(cl.notes)              # but a human-judgment note is emitted

    def test_generated_file_edit_hard_fails(self):
        rel = ".ruse/test_generated.md"
        p = repo.path(rel)
        os.makedirs(os.path.dirname(p), exist_ok=True)
        with open(p, "w", encoding="utf-8") as fh:
            fh.write("<!-- GENERATED FILE: DO NOT EDIT -->\nx\n")
        try:
            cl = classify_changeset([rel], "docs-editorial")
            self.assertIn(rel, cl.generated_hits)
            self.assertFalse(cl.ok)
        finally:
            os.remove(p)


if __name__ == "__main__":
    unittest.main()
