import os
import unittest

from . import _support  # noqa: F401  (sys.path side effect)
from rusekit import repo  # noqa: E402
from rusekit.change.classify import classify_changeset  # noqa: E402


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

    def test_a_stability_contract_forces_contract(self):
        # #884: the `contract` floor pointed at crates/plugin-protocol/, which does not exist. The contracts
        # themselves live in spec/contracts/; implementation (risk 2) is below that floor (risk 3).
        for f in ("spec/contracts/plugin-protocol.yaml", "spec/contracts/remote-runtime.yaml",
                  "spec/contracts/persistence-format.yaml"):
            cl = classify_changeset([f], "implementation")
            self.assertEqual(cl.observed_kind, "contract", f)
            self.assertFalse(cl.ok, f)

    def test_every_path_trigger_matches_a_real_path(self):
        # A floor on a path that does not exist guards nothing (#884). Triggers for files a change may
        # create (root config that is not there yet) are listed explicitly.
        import re
        from rusekit import contract
        code, out = repo._git("ls-files")
        if code != 0:
            self.skipTest("not a git checkout")
        files = out.splitlines()
        optional = {r"^\.cargo/"}
        for t in contract.load_kinds().get("path_triggers") or []:
            if t["pattern"] in optional:
                continue
            self.assertTrue(any(re.search(t["pattern"], f) for f in files),
                            f"path_trigger {t['pattern']} matches no file in the repo")

    def test_gate_policy_files_are_not_editorial(self):
        # #884: each of these loosens or tightens a gate, so none may pass as docs-editorial (risk 0).
        cases = {
            "spec/waivers.yaml": "spec", "spec/parity/ratchet.yaml": "spec",
            "spec/verification.yaml": "spec", "spec/fitness.yaml": "spec",
            "spec/perf-baseline.yaml": "spec", "spec/parity/upstreams.yaml": "spec",
            "spec/templates/change.yaml": "spec",
            "spec/architecture.yaml": "architecture",
            "clippy.toml": "build", "rust-toolchain.toml": "build", "renovate.json": "build",
            "scripts/bootstrap.sh": "build",
            "tests/parity/vim/motions/corpus.yaml": "implementation",
            "tests/unicode/x.txt": "implementation",
        }
        for f, kind in cases.items():
            cl = classify_changeset([f], "docs-editorial")
            self.assertEqual(cl.observed_kind, kind, f)
            self.assertFalse(cl.ok, f)

    def test_spec_prose_digest_stays_human_judged(self):
        # The catch-all is for machine-read spec YAML only; spec/CONTEXT.md is a hand digest.
        cl = classify_changeset(["spec/CONTEXT.md"], "docs-editorial")
        self.assertIsNone(cl.observed_kind)
        self.assertTrue(cl.ok)

    def test_docs_prose_is_human_judgment_not_a_floor(self):
        cl = classify_changeset(["docs/design/document-model.md"], "docs-editorial")
        self.assertTrue(cl.ok)                 # no forced floor
        self.assertTrue(cl.notes)              # but a human-judgment note is emitted

    def test_github_config_is_build(self):
        cl = classify_changeset([".github/workflows/ci.yml"], "build")
        self.assertEqual(cl.observed_kind, "build")
        self.assertTrue(cl.ok)

    def test_root_manifests_are_build(self):
        for f in ("Cargo.toml", "Cargo.lock", "pyproject.toml", "lefthook.yml"):
            cl = classify_changeset([f], "build")
            self.assertEqual(cl.observed_kind, "build", f)
            self.assertTrue(cl.ok, f)

    def test_docs_editorial_below_build_fails(self):
        # a CI change under-declared as docs-editorial (risk 0) must be raised to build (risk 1).
        cl = classify_changeset([".github/labeler.yml"], "docs-editorial")
        self.assertEqual(cl.observed_kind, "build")
        self.assertFalse(cl.ok)

    def test_crate_manifest_stays_implementation(self):
        # a per-crate Cargo.toml is crate source territory, not a root build manifest.
        cl = classify_changeset(["crates/core/Cargo.toml"], "build")
        self.assertEqual(cl.observed_kind, "implementation")
        self.assertFalse(cl.ok)

    def test_generated_file_edit_hard_fails(self):
        rel = "target/rusekit-test_generated.md"  # gitignored scratch, never .ruse/ (D-062)
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
