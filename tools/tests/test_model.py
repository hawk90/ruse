import unittest

from . import _support  # noqa: F401
from rusekit import model as model_mod  # noqa: E402


class TestModel(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.m = model_mod.load()

    def test_loads_without_errors(self):
        self.assertEqual(self.m.errors, [], f"model load errors: {self.m.errors}")

    def test_core_ids_present(self):
        for nid in ("CAP-EDIT-CORE", "F-001", "C-TRANSACTION", "INV-TXN", "D-001"):
            self.assertTrue(self.m.has(nid), f"missing {nid}")

    def test_capability_reaches_its_features(self):
        reach = self.m.bfs(["CAP-EDIT-CORE"], depth=2)
        self.assertIn("F-001", reach)          # CAP -prd-> F-001

    def test_file_maps_to_crate_node(self):
        ids = self.m.ids_for_file("crates/core/src/lib.rs")
        self.assertIn("CRATE-core", ids)

    def test_dependency_lives_in_declared_crate(self):
        # DEP-ROPE allowed_layers: [core] -> edge to CRATE-core
        rope = self.m.neighbors("DEP-ROPE", direction="out", rels={"layer"})
        targets = {e.dst for e in rope}
        self.assertIn("CRATE-core", targets)


if __name__ == "__main__":
    unittest.main()
