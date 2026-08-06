import unittest

from . import _support  # noqa: F401
from rusekit import repo  # noqa: E402
from rusekit.arch.dependencies import _closure, _find_cycle  # noqa: E402

import yaml  # noqa: E402


class TestArchitectureContract(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.arch = yaml.safe_load(open(repo.path("spec/architecture.yaml")))
        cls.crates = cls.arch["crates"]

    def test_declared_crates_are_real(self):
        for c in self.crates:
            self.assertIn(c, repo.CRATES, f"architecture.yaml names unknown crate {c}")

    def test_kernel_depends_on_nothing(self):
        self.assertEqual(self.crates["core"].get("may_depend_on"), [])

    def test_plugin_protocol_is_isolated(self):
        # trust boundary — must not reach core (INV-PLUGIN-NO-CORE)
        self.assertEqual(self.crates["plugin-protocol"].get("may_depend_on"), [])

    def test_transitive_closure(self):
        cl = _closure(self.crates)
        self.assertEqual(cl["core"], set())
        self.assertEqual(cl["render-model"], {"core"})
        self.assertEqual(cl["terminal-platform"], {"core", "render-model"})
        self.assertEqual(cl["workspace-runtime"], {"core", "workspace", "plugin-protocol"})

    def test_declared_contract_is_acyclic(self):
        g = {c: list(v.get("may_depend_on") or []) for c, v in self.crates.items()}
        self.assertIsNone(_find_cycle(g), "architecture.yaml declares a crate cycle")

    def test_forbidden_module_edges_present(self):
        self.assertTrue(self.arch.get("forbidden_module_edges"), "ARCH-FORBID-001 edges not encoded")


if __name__ == "__main__":
    unittest.main()
