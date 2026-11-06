"""#886: a done feature's trace paths exist. #556: the dependency-sync hooks fire only when they should."""
import os
import shutil
import stat
import subprocess
import tempfile
import unittest

from . import _support  # noqa: F401
from rusekit import repo  # noqa: E402
from rusekit.spec_trace import done_trace_errors  # noqa: E402

try:
    import yaml
except ImportError:  # pragma: no cover
    yaml = None

SPEC = repo.path("spec")


class TestDoneTracePaths(unittest.TestCase):
    def test_a_missing_verify_path_of_a_done_feature_fails(self):
        feats = {"F-1": {"status": "done", "trace": {"verify": ["../tests/unit/no-such-test"]}}}
        errs = done_trace_errors(feats, SPEC)
        self.assertEqual(len(errs), 1)
        self.assertIn("trace.verify does not resolve", errs[0])

    def test_a_missing_contract_path_of_a_done_feature_fails(self):
        feats = {"F-1": {"status": "done", "trace": {"verify": ["../crates/core/tests/properties.rs"],
                                                    "contract": ["../spec/contracts/nope.yaml"]}}}
        self.assertEqual(len(done_trace_errors(feats, SPEC)), 1)

    def test_a_done_feature_without_verify_fails(self):
        self.assertEqual(len(done_trace_errors({"F-1": {"status": "done", "trace": {}}}, SPEC)), 1)

    def test_planned_paths_are_not_checked(self):
        feats = {"F-1": {"status": "planned", "trace": {"verify": ["../tests/unit/later"]}}}
        self.assertEqual(done_trace_errors(feats, SPEC), [])

    def test_real_paths_with_a_fragment_pass(self):
        feats = {"F-1": {"status": "done",
                         "trace": {"verify": ["../crates/core/tests/properties.rs#undo",
                                              "../tests/parity/vim/fixtures/corpus.yaml"]}}}
        self.assertEqual(done_trace_errors(feats, SPEC), [])

    @unittest.skipIf(yaml is None, "PyYAML required")
    def test_the_repo_prd_passes(self):
        with open(repo.path("spec", "PRD.yaml"), encoding="utf-8") as fh:
            feats = (yaml.safe_load(fh) or {}).get("features") or {}
        self.assertEqual(done_trace_errors(feats, SPEC), [])


@unittest.skipIf(yaml is None or shutil.which("sh") is None, "PyYAML and sh required")
class TestSyncDepsHooks(unittest.TestCase):
    """Run lefthook.yml's sync-deps commands with lefthook's {N} substitution, a stub sync-deps.sh and a
    stub `git` whose --git-path points into a temp dir."""

    def _run(self, hook, args, rebase_dir=None):
        with open(repo.path("lefthook.yml"), encoding="utf-8") as fh:
            cmd = yaml.safe_load(fh)[hook]["commands"]["sync-deps"]["run"]
        for i, a in enumerate(args, 1):
            cmd = cmd.replace("{%d}" % i, a)
        with tempfile.TemporaryDirectory() as d:
            os.makedirs(os.path.join(d, "scripts"))
            os.makedirs(os.path.join(d, "bin"))
            gitdir = os.path.join(d, "gitdir")
            os.makedirs(gitdir)
            if rebase_dir:
                os.makedirs(os.path.join(gitdir, rebase_dir))
            marker = os.path.join(d, "ran")
            for path, body in ((os.path.join(d, "scripts", "sync-deps.sh"), f'echo "$@" > "{marker}"\n'),
                               (os.path.join(d, "bin", "git"), f'[ "$2" = --git-path ] && echo "{gitdir}/$3"\n')):
                with open(path, "w") as fh:
                    fh.write("#!/bin/sh\n" + body)
                os.chmod(path, os.stat(path).st_mode | stat.S_IEXEC)
            env = dict(os.environ, PATH=os.path.join(d, "bin") + os.pathsep + os.environ.get("PATH", ""))
            subprocess.run(["sh", "-c", cmd], cwd=d, env=env, check=True, timeout=30)
            if not os.path.exists(marker):
                return None
            with open(marker) as fh:
                return fh.read().strip()

    def test_post_rewrite_syncs_after_a_rebase_only(self):
        self.assertEqual(self._run("post-rewrite", ["rebase"]), "")
        self.assertIsNone(self._run("post-rewrite", ["amend"]))

    def test_post_checkout_syncs_on_a_branch_checkout(self):
        self.assertEqual(self._run("post-checkout", ["abc123", "def456", "1"]), "--since abc123")

    def test_post_checkout_skips_a_file_checkout(self):
        self.assertIsNone(self._run("post-checkout", ["abc123", "abc123", "0"]))

    def test_post_checkout_skips_inside_a_rebase(self):
        for d in ("rebase-merge", "rebase-apply"):
            self.assertIsNone(self._run("post-checkout", ["abc123", "def456", "1"], rebase_dir=d), d)


class TestBootstrap(unittest.TestCase):
    def test_bootstrap_does_not_run_corepack(self):
        # #556: `corepack enable` replaced a globally installed pnpm with its shim.
        with open(repo.path("scripts", "bootstrap.sh"), encoding="utf-8") as fh:
            code = [ln for ln in fh if not ln.lstrip().startswith("#")]
        self.assertFalse(any("corepack" in ln for ln in code))


if __name__ == "__main__":
    unittest.main()
