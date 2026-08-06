import io
import os
import tempfile
import unittest
from contextlib import redirect_stdout

from . import _support  # noqa: F401  (sys.path side effect)
from rusekit.change import pr_check  # noqa: E402


def _run(files, actor):
    """Run `pr check` in CI mode with a body that has NO gate block, over an explicit file list."""
    with tempfile.NamedTemporaryFile("w", suffix=".md", delete=False) as fh:
        fh.write("Automated PR body — intentionally no ruse-gate:v1 block.\n")
        body = fh.name
    try:
        args = ["--pr-body", body, "--files", *files]
        if actor is not None:
            args += ["--actor", actor]
        with redirect_stdout(io.StringIO()):
            return pr_check.main(args)
    finally:
        os.remove(body)


class TestPrCheckBotGate(unittest.TestCase):
    def test_untrusted_author_without_block_fails(self):
        # A human PR with no gate block is rejected (the gate of record needs the declaration).
        self.assertEqual(_run([".github/labeler.yml"], actor="some-human"), 1)

    def test_missing_actor_without_block_fails(self):
        self.assertEqual(_run([".github/labeler.yml"], actor=None), 1)

    def test_trusted_bot_build_change_passes(self):
        # Dependabot bumping a CI action → derived kind 'build' (no required artifacts) → passes.
        self.assertEqual(_run([".github/workflows/ci.yml"], actor="dependabot[bot]"), 0)

    def test_trusted_bot_straying_into_code_still_fails(self):
        # Safety: a bot diff that reaches implementation territory needs an `issue` artifact it
        # cannot supply, so the auto-declared contract still FAILS the artifact gate.
        self.assertEqual(_run(["crates/core/src/lib.rs"], actor="dependabot[bot]"), 1)


if __name__ == "__main__":
    unittest.main()
