import contextlib
import importlib.util
import io
import os
import tempfile
import unittest

from . import _support  # noqa: F401
from rusekit import repo  # noqa: E402

# lint-commit-msg.py has a hyphen — load it by path.
_spec = importlib.util.spec_from_file_location(
    "lint_commit_msg", os.path.join(repo.ROOT, "tools", "lint-commit-msg.py"))
lcm = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(lcm)


def _run(msg: str) -> int:
    with tempfile.NamedTemporaryFile("w", suffix=".txt", delete=False) as fh:
        fh.write(msg)
        path = fh.name
    try:
        with contextlib.redirect_stdout(io.StringIO()):   # swallow the tool's report
            return lcm.main([path])
    finally:
        os.remove(path)


class TestCommitMsg(unittest.TestCase):
    def test_allowed_types_sourced_from_doc(self):
        types = lcm.allowed_types()
        for t in ("spec", "rfc", "feat", "fix", "docs", "build", "chore"):
            self.assertIn(t, types)

    def test_good_subjects_pass(self):
        for msg in ("feat(core): add transaction apply path",
                    "docs: document the change workflow",
                    "fix(input): reject stale revision",
                    "Merge branch 'main'"):
            self.assertEqual(_run(msg), 0, f"should pass: {msg}")

    def test_bad_subjects_fail(self):
        for msg in ("update ruse.py",
                    "feature: add thing",            # unknown type
                    "fix: ",                         # empty desc
                    "docs: has a trailing period."):
            self.assertEqual(_run(msg), 1, f"should fail: {msg}")

    def test_overlong_subject_fails(self):
        self.assertEqual(_run("chore: " + "x" * 80), 1)


if __name__ == "__main__":
    unittest.main()
