import unittest

from . import _support  # noqa: F401
from rusekit import repo  # noqa: E402
from rusekit.arch import source_scan  # noqa: E402
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

    def test_collapsed_to_core_only(self):
        # D-039 / RFC-0012: the workspace is a single crate (core); deferred boundaries are not crates.
        self.assertEqual(set(self.crates), {"core"})

    def test_transitive_closure(self):
        cl = _closure(self.crates)
        self.assertEqual(cl["core"], set())

    def test_declared_contract_is_acyclic(self):
        g = {c: list(v.get("may_depend_on") or []) for c, v in self.crates.items()}
        self.assertIsNone(_find_cycle(g), "architecture.yaml declares a crate cycle")

    def test_forbidden_module_edges_present(self):
        self.assertTrue(self.arch.get("forbidden_module_edges"), "ARCH-FORBID-001 edges not encoded")

    def test_every_forbidden_edge_is_source_checked(self):
        # #983: an edge without a scan would be declared but never enforced.
        for e in self.arch["forbidden_module_edges"]:
            self.assertTrue((e.get("scan") or {}).get("forbid"), f"edge {e} has no scan")

    def test_the_tree_passes_the_source_rules(self):
        errors, _ = source_scan.check(self.arch)
        self.assertEqual(errors, [])


class TestSourceScan(unittest.TestCase):
    def test_test_items_are_blanked_and_lines_kept(self):
        code = "fn a() {}\n#[cfg(test)]\nmod t {\n    fn b() { Transaction::new() }\n}\nfn c() {}\n"
        out = source_scan.blank_test_items(code)
        self.assertNotIn("Transaction", out)
        self.assertEqual(out.count("\n"), code.count("\n"))
        self.assertIn("fn c()", out)

    def test_a_cfg_test_declaration_is_blanked(self):
        out = source_scan.blank_test_items("#[cfg(test)]\nmod tests;\nfn keep() {}\n")
        self.assertNotIn("tests", out)
        self.assertIn("keep", out)

    def test_the_scan_finds_a_mutation_outside_the_allowed_files(self):
        # The editor builds transactions; without it on the allow list the rule must see them.
        t = yaml.safe_load(open(repo.path("spec/architecture.yaml")))["txn_only_mutation"]
        self.assertTrue(source_scan.scan(["crates/core/src/editor/mod.rs"], t["forbid"]))
        self.assertFalse(source_scan.scan(["crates/core/src/editor/mod.rs"], t["forbid"], t["allow"]))

    def test_comments_and_strings_do_not_count(self):
        # document.rs mentions "View" only in its docs.
        self.assertFalse(source_scan.scan(["crates/core/src/document.rs"], [r"\bView\b"]))


if __name__ == "__main__":
    unittest.main()
