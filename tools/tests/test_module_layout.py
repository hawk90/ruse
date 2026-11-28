"""rusekit.docs.module_layout: docs/contributing/module-layout.md is re-measured (#1689)."""
import os
import shutil
import tempfile
import unittest

from . import _support  # noqa: F401
from rusekit import repo  # noqa: E402
from rusekit.docs import module_layout as ml  # noqa: E402


def _touch(root, rel, text=""):
    p = os.path.join(root, rel)
    os.makedirs(os.path.dirname(p), exist_ok=True)
    with open(p, "w", encoding="utf-8") as fh:
        fh.write(text)


class MeasureTest(unittest.TestCase):
    def setUp(self):
        self.root = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, self.root)
        core, tui = "crates/core/src/", "apps/tui/src/"
        # crates/core: editor/mod.rs (mod), editor/commit.rs + commit/ (x), editor/tests hub
        _touch(self.root, core + "lib.rs", "#[cfg(test)]\nmod tests {\n}\n")
        _touch(self.root, core + "editor/mod.rs")
        _touch(self.root, core + "editor/commit.rs", "    mod tests {}\n")
        _touch(self.root, core + "editor/commit/plan.rs")
        _touch(self.root, core + "editor/tests.rs")
        _touch(self.root, core + "editor/tests/motion_tests.rs")
        _touch(self.root, core + "editor/tests/undo_tests.rs")
        # apps/tui: ui/render.rs + render/ with a topic suite beside the module, plus a tests.rs
        _touch(self.root, tui + "ui/render.rs")
        _touch(self.root, tui + "ui/render/caret_tests.rs")
        _touch(self.root, tui + "ui/render/tests.rs")

    def test_counts(self):
        f = ml.measure(self.root)
        self.assertEqual(f["mod_dirs"], {"crates/core/src/editor/"})
        self.assertEqual((f["x_nontest_core"], f["x_test_core"], f["mod_core"]), (1, 1, 1))
        self.assertEqual((f["x_nontest_tui"], f["x_test_tui"], f["mod_tui"]), (1, 0, 0))
        self.assertEqual(f["nontest_total"], 3)
        self.assertEqual((f["t1_core"], f["t1_tui"]), (2, 0))
        self.assertEqual((f["t2_total"], f["t2_hubs"]), (2, 1))
        self.assertEqual((f["t3_beside"], f["t3_under"]), (1, 2))
        self.assertEqual(f["hub_topics"], {"crates/core/src/editor/tests": 2})
        self.assertEqual(f["beside_dirs"], {("apps/tui", "ui/render/")})


class CheckTest(unittest.TestCase):
    """`check` against the real doc with doctored facts, so the patterns are the shipped ones."""

    def setUp(self):
        with open(repo.path(ml.DOC), encoding="utf-8") as fh:
            self.text = fh.read()
        self.facts = ml.measure(repo.ROOT)

    def check(self, text=None, **facts):
        return ml.check(self.text if text is None else text, repo.ROOT, dict(self.facts, **facts))

    def test_the_doc_on_main_is_current(self):
        errors, warns, fixed = self.check()
        self.assertEqual((errors, warns), ([], []))
        self.assertEqual(fixed, self.text)

    def test_a_volatile_tally_only_warns_and_write_fixes_it(self):
        stale = {"t2_total": self.facts["t2_total"] + 1}
        errors, warns, fixed = self.check(**stale)
        self.assertEqual(errors, [])
        self.assertEqual(len(warns), 1, warns)
        self.assertTrue(warns[0].startswith(ml.DOC + ":") and "t2_total" in warns[0])
        self.assertIn("--write", warns[0])
        self.assertEqual(self.check(fixed, **stale)[:2], ([], []))

    def test_a_structural_count_is_an_error(self):
        errors, warns, _ = self.check(mod_tui=self.facts["mod_tui"] + 1)
        self.assertEqual(warns, [])
        self.assertEqual(len(errors), 1, errors)
        self.assertIn("mod_tui", errors[0])

    def test_a_reworded_claim_is_an_error_not_a_silent_pass(self):
        errors, _, _ = self.check(self.text.replace("directories, listed below", "directories, below"))
        self.assertEqual(len(errors), 1)
        self.assertIn("no text matches", errors[0])

    def test_a_table_that_no_longer_parses_is_an_error(self):
        text = self.text
        for hub in ("crates/core/src/editor/tests", "crates/core/src/workspace/tests",
                    "apps/tui/src/input/unit_tests"):
            text = text.replace(f"  | `{hub}.rs`", f"  | {hub}.rs")
        errors, _, _ = self.check(text)
        self.assertIn("no T3 hub table row found", errors[0])

    def test_hub_set_is_an_error_and_topic_counts_warn(self):
        hub = "apps/tui/src/input/unit_tests"
        errors, warns, fixed = self.check(hub_topics=dict(self.facts["hub_topics"], **{hub: 99}))
        self.assertEqual(errors, [])
        self.assertEqual(len(warns), 1)
        self.assertIn(hub, warns[0])
        self.assertIn("| 99 |", fixed)
        extra = dict(self.facts["hub_topics"], **{"apps/tui/src/x/tests": 2})
        errors, _, _ = self.check(hub_topics=extra)
        self.assertEqual(len(errors), 1, errors)
        self.assertIn("does not list the test hub apps/tui/src/x/tests.rs", errors[0])

    def test_the_t4_table_rows_must_match_its_count(self):
        text = self.text.replace("| `apps/tui/src/persist/atomic/fault.rs` |", "| fault.rs |")
        errors, _, _ = self.check(text)
        self.assertEqual(len(errors), 1, errors)
        self.assertIn("t4_exceptions says 3, measured 2", errors[0])

    def test_the_named_directory_lists_must_match(self):
        errors, _, _ = self.check(mod_dirs=self.facts["mod_dirs"] | {"apps/tui/src/new/"},
                                  beside_dirs=self.facts["beside_dirs"] - {("apps/tui", "highlight/")})
        self.assertEqual(len(errors), 2, errors)
        self.assertIn("does not list the `x/mod.rs` directory apps/tui/src/new/", errors[0])
        self.assertIn("apps/tui/src/highlight/ has no", errors[1])


if __name__ == "__main__":
    unittest.main()
