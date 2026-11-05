"""#1204: the contracts.* flags and artifacts.migration are settable, rendered and enforced."""
import argparse
import io
import json
import os
import tempfile
import unittest
from contextlib import redirect_stdout

from . import _support  # noqa: F401
from rusekit import repo  # noqa: E402
from rusekit.change import edit, pr_check, pr_render  # noqa: E402
from rusekit.change.classify import classify_changeset  # noqa: E402

try:
    import yaml
except ImportError:  # pragma: no cover
    yaml = None


def _args(argv):
    ap = argparse.ArgumentParser()
    edit.add_arguments(ap)
    return ap.parse_args(argv)


def _template():
    with open(repo.path("spec", "templates", "change.yaml"), encoding="utf-8") as fh:
        return fh.read()


@unittest.skipIf(yaml is None, "PyYAML required")
class TestEditFlags(unittest.TestCase):
    def test_persistent_format_and_migration_are_set_in_place(self):
        text, changes = edit.apply(_template(), _args(
            ["--persistent-format", "--migration", "RFC-0023 §3"]))
        data = yaml.safe_load(text)
        self.assertIs(data["contracts"]["persistent_format"], True)
        self.assertEqual(data["artifacts"]["migration"], "RFC-0023 §3")
        self.assertIn("contracts.persistent_format = true", changes)
        self.assertIn("# contracts", text, "the template's comments survive")

    def test_no_flag_clears_it_and_other_flags_are_untouched(self):
        text, _ = edit.apply(_template(), _args(["--protocol"]))
        text, _ = edit.apply(text, _args(["--no-protocol", "--public-api"]))
        con = yaml.safe_load(text)["contracts"]
        self.assertEqual(con, {"public_api": True, "protocol": False, "persistent_format": False})

    def test_an_empty_migration_is_refused(self):
        with self.assertRaises(ValueError):
            edit.apply(_template(), _args(["--migration", "  "]))

    def test_render_shows_the_migration_reference(self):
        self.assertEqual(pr_render._migration("RFC-0023 §3"), "RFC-0023 §3")
        self.assertEqual(pr_render._migration(False), "—")


class TestClassifyFlagHits(unittest.TestCase):
    def test_format_paths_suggest_the_flag(self):
        files = ["spec/contracts/persistence-format.yaml", "apps/tui/src/persist/rj01.rs",
                 "apps/tui/src/persist/journal.rs"]
        cl = classify_changeset(files, "implementation")
        self.assertEqual([(f, flag) for f, flag, _ in cl.flag_hits],
                         [("spec/contracts/persistence-format.yaml", "persistent_format"),
                          ("apps/tui/src/persist/rj01.rs", "persistent_format")])


def _gate(kind, contracts=None, artifacts=None, allow=("apps/",)):
    art = {"issue": "1204", "rfc": None, "decision": None, "migration": False}
    art.update(artifacts or {})
    con = {"public_api": False, "protocol": False, "persistent_format": False}
    con.update(contracts or {})
    block = {"v": 1, "kind": kind, "goal": "x", "affected": {"requirements": ["F-008"]},
             "contracts": con, "artifacts": art, "allow_paths": list(allow), "forbid_paths": []}
    return "Refs #1204\n\n<!-- ruse-gate:v1 -->\n```json\n" + json.dumps(block) + "\n```\n"


def _check(body, files):
    with tempfile.NamedTemporaryFile("w", suffix=".md", delete=False, encoding="utf-8") as fh:
        fh.write(body)
        path = fh.name
    out = io.StringIO()
    try:
        with redirect_stdout(out):
            code = pr_check.main(["--pr-body", path, "--files", *files])
    finally:
        os.remove(path)
    return code, out.getvalue()


class TestPrCheckHonoursFlags(unittest.TestCase):
    FILES = ["apps/tui/src/persist/rj01.rs"]

    def test_a_format_change_declared_as_implementation_fails(self):
        code, out = _check(_gate("implementation", {"persistent_format": True},
                                 {"migration": "RFC-0023 §3"}), self.FILES)
        self.assertEqual(code, 1, out)
        self.assertIn("too low", out)

    def test_a_format_change_needs_its_migration(self):
        code, out = _check(_gate("contract", {"persistent_format": True}, {"rfc": "RFC-0023"}),
                           self.FILES)
        self.assertEqual(code, 1, out)
        self.assertIn("--migration", out)

    def test_a_complete_format_change_passes(self):
        code, out = _check(_gate("contract", {"persistent_format": True},
                                 {"rfc": "RFC-0023", "migration": "RFC-0023 §3"}), self.FILES)
        self.assertEqual(code, 0, out)
        self.assertIn("names its migration (RFC-0023 §3)", out)

    def test_an_unflagged_format_path_only_warns(self):
        code, out = _check(_gate("implementation"), self.FILES)
        self.assertEqual(code, 0, out)
        self.assertIn("--persistent-format", out)


if __name__ == "__main__":
    unittest.main()
