"""#884/#885/#887/#888: gate and verify checks fail safe — an unknown input is a failure, never a pass."""
import io
import os
import shutil
import stat
import subprocess
import tempfile
import unittest
from contextlib import redirect_stdout
from unittest import mock

from . import _support  # noqa: F401
from rusekit import repo  # noqa: E402
from rusekit.arch import dependencies  # noqa: E402
from rusekit.gov import fitness, ratchet  # noqa: E402
from rusekit.verify import run  # noqa: E402

try:
    import yaml
except ImportError:  # pragma: no cover
    yaml = None


def _quiet(fn, *a, **kw):
    out = io.StringIO()
    with redirect_stdout(out):
        return fn(*a, **kw), out.getvalue()


@unittest.skipUnless(repo.is_git_repo(), "needs a git checkout")
class TestChangedFiles(unittest.TestCase):
    def test_a_failed_base_diff_is_an_error_not_an_empty_set(self):
        # #885: `--base origin/no-such-branch` gave "Changed files: 0" and PASS.
        with self.assertRaises(repo.ChangedFilesError) as cm:
            repo.changed_files("origin/no-such-branch-885")
        self.assertIn("no-such-branch-885", str(cm.exception.code))

    def test_classify_on_a_bad_base_does_not_pass(self):
        from rusekit.change import classify
        with self.assertRaises(SystemExit) as cm:
            _quiet(classify.main, ["--kind", "docs-editorial", "--base", "origin/no-such-branch-885"])
        self.assertNotEqual(cm.exception.code, 0)

    def test_a_good_base_still_lists_files(self):
        self.assertIsInstance(repo.changed_files("HEAD"), list)


class TestFitnessChecker(unittest.TestCase):
    LIVE = {"id": "FIT-X", "metric": "m", "operator": "equals", "threshold": 0,
            "kind": "holistic", "status": "live"}

    def test_an_unknown_checker_fails(self):
        # #887: `check: no-such-checker` passed.
        e, _w, _c = fitness.validate([dict(self.LIVE, check="no-such-checker")])
        self.assertTrue(any("no-such-checker" in x for x in e), e)

    def test_verify_steps_and_gov_modules_are_known(self):
        for chk in ("dependency-check", "waivers", "ratchet"):
            e, _w, _c = fitness.validate([dict(self.LIVE, check=chk)])
            self.assertEqual(e, [], chk)


class TestRatchetRaise(unittest.TestCase):
    BASE = {"buckets": {"backlog": 77}, "raised": {}}

    def test_a_raised_ceiling_without_a_reason_fails(self):
        # #884: a ceiling raised to 9999 passed with only a "lower it" warning.
        head = {"buckets": {"backlog": 9999}, "raised": {}}
        self.assertEqual(len(ratchet.unexplained_raises(self.BASE, head, {"backlog": 76})), 1)

    def test_a_raise_with_a_new_reason_passes(self):
        head = {"buckets": {"backlog": 80}, "raised": {"backlog": "#999 three new census rows"}}
        self.assertEqual(ratchet.unexplained_raises(self.BASE, head, {"backlog": 80}), [])

    def test_a_reason_already_on_the_base_does_not_excuse_a_second_raise(self):
        base = {"buckets": {"backlog": 80}, "raised": {"backlog": "#999 three new census rows"}}
        head = {"buckets": {"backlog": 90}, "raised": {"backlog": "#999 three new census rows"}}
        self.assertEqual(len(ratchet.unexplained_raises(base, head, {"backlog": 90})), 1)

    def test_lowering_and_keeping_pass(self):
        for ceil in (77, 70):
            head = {"buckets": {"backlog": ceil}}
            self.assertEqual(ratchet.unexplained_raises(self.BASE, head, {"backlog": 70}), [])

    def test_a_new_bucket_may_not_start_above_its_count(self):
        head = {"buckets": {"backlog": 77, "fresh": 50}}
        self.assertEqual(len(ratchet.unexplained_raises(self.BASE, head, {"backlog": 77, "fresh": 10})), 1)
        head = {"buckets": {"backlog": 77, "fresh": 10}}
        self.assertEqual(ratchet.unexplained_raises(self.BASE, head, {"backlog": 77, "fresh": 10}), [])

    def test_the_repo_ratchet_passes(self):
        code, out = _quiet(ratchet.main, [])
        self.assertEqual(code, 0, out)

    def test_every_bucket_has_a_ceiling(self):
        buckets = (ratchet._load(ratchet.BASELINE).get("buckets")) or {}
        self.assertEqual(sorted(ratchet.current_buckets()), sorted(buckets))


@unittest.skipIf(yaml is None, "PyYAML required")
class TestVerifyRegistry(unittest.TestCase):
    def _registry(self, text):
        fh = tempfile.NamedTemporaryFile("w", suffix=".yaml", delete=False, encoding="utf-8")
        fh.write(text)
        fh.close()
        self.addCleanup(os.remove, fh.name)
        return fh.name

    def test_a_malformed_registry_is_an_error(self):
        # #888: a broad `except` fell back to spec-validate alone and still reported a pass.
        for text in ("steps: [", "version: 1\n", "steps:\n  - { name: x }\n",
                     "steps:\n  - { name: x, scope: nope, cmd: [true] }\n"):
            with self.assertRaises(run.RegistryError, msg=text):
                run._load_steps(self._registry(text))

    def test_a_missing_registry_is_an_error(self):
        with self.assertRaises(run.RegistryError):
            run._load_steps("/nonexistent/verification.yaml")

    def test_the_repo_registry_loads(self):
        names = [s["name"] for s in run._load_steps()]
        self.assertIn("cargo-test", names)

    def test_a_needed_step_with_a_missing_tool_cannot_pass(self):
        # #888: a missing `cargo` was a warning and `verify --full` stayed green.
        steps = [{"name": "x", "scope": "rust", "needs": "no-such-tool-888", "heavy": False,
                  "gate_on": None, "cmd": ["no-such-tool-888"]}]
        selected, skipped, missing = run._select(steps, {"rust"}, heavy=True)
        self.assertEqual((selected, skipped), ([], []))
        self.assertEqual(len(missing), 1)

    def test_main_fails_when_a_tool_is_missing(self):
        steps = [{"name": "x", "scope": "rust", "needs": "no-such-tool-888", "heavy": False,
                  "gate_on": None, "cmd": ["no-such-tool-888"]}]
        with mock.patch.object(run, "_load_steps", return_value=steps):
            code, out = _quiet(run.main, ["--full", "--no-record", "--files", "x.rs"])
        self.assertEqual(code, 1, out)

    def test_main_fails_on_a_bad_registry(self):
        with mock.patch.object(run, "_load_steps", side_effect=run.RegistryError("bad")):
            code, _ = _quiet(run.main, ["--no-record", "--files", "x.rs"])
        self.assertEqual(code, 2)


class TestVerifyScope(unittest.TestCase):
    def test_rust_config_selects_rust(self):
        for f in ("clippy.toml", "rust-toolchain.toml", "Cargo.toml", "Cargo.lock", "tests/parity/a.yaml",
                  "fuzz/fuzz_targets/x.rs", ".cargo/config.toml"):
            self.assertIn("rust", run._scope([f], full=False), f)

    def test_an_unrecognised_file_selects_every_scope(self):
        self.assertEqual(run._scope(["scripts/bootstrap.sh"], full=False), run.SCOPES)

    def test_docs_stay_docs(self):
        self.assertEqual(run._scope(["docs/design/x.md"], full=False), {"spec_docs"})


@unittest.skipIf(yaml is None, "PyYAML required")
class TestArchDepsNeedsItsInputs(unittest.TestCase):
    def test_no_pyyaml_fails(self):
        # #888 (comment): without PyYAML `arch deps` reported PASS with the direction checks skipped.
        with mock.patch.object(dependencies, "yaml", None):
            code, out = _quiet(dependencies.main, [])
        self.assertEqual(code, 1, out)

    def test_no_cargo_graph_fails(self):
        with mock.patch.object(dependencies, "_cargo_metadata", return_value=None):
            code, out = _quiet(dependencies.main, [])
        self.assertEqual(code, 1, out)
        self.assertIn("cargo metadata unavailable", out)


@unittest.skipIf(yaml is None or shutil.which("bash") is None, "PyYAML and bash required")
class TestCiDetect(unittest.TestCase):
    """The `detect` step of .github/workflows/ci.yml, run with a stub `git` (#888)."""

    STUB = """#!/bin/sh
case "$1" in
  fetch) exit 0 ;;
  diff) if [ -n "$STUB_DIFF_FAIL" ]; then echo "fatal: bad object" >&2; exit 128; fi
        printf '%s\\n' $STUB_FILES; exit 0 ;;
esac
exit 0
"""

    def _detect(self, files="", fail=False):
        with open(repo.path(".github", "workflows", "ci.yml"), encoding="utf-8") as fh:
            wf = yaml.safe_load(fh)
        script = next(s["run"] for s in wf["jobs"]["rust"]["steps"] if s.get("id") == "detect")
        with tempfile.TemporaryDirectory() as d:
            stub = os.path.join(d, "bin")
            os.mkdir(stub)
            g = os.path.join(stub, "git")
            with open(g, "w") as fh:
                fh.write(self.STUB)
            os.chmod(g, os.stat(g).st_mode | stat.S_IEXEC)
            open(os.path.join(d, "Cargo.toml"), "w").close()
            out = os.path.join(d, "out")
            env = dict(os.environ, PATH=stub + os.pathsep + os.environ.get("PATH", ""),
                       EVENT="pull_request", BASE_REF="main", GITHUB_OUTPUT=out, STUB_FILES=files)
            if fail:
                env["STUB_DIFF_FAIL"] = "1"
            subprocess.run(["bash", "-e", "-c", script], cwd=d, env=env, check=True,
                           capture_output=True, timeout=30)
            with open(out) as fh:
                return dict(ln.split("=", 1) for ln in fh.read().split())

    def test_a_failed_diff_runs_the_rust_steps(self):
        self.assertEqual(self._detect(fail=True)["rust"], "1")

    def test_a_docs_only_diff_skips_them(self):
        self.assertEqual(self._detect("docs/a.md README.md")["rust"], "0")

    def test_lint_and_toolchain_config_runs_them(self):
        for f in ("clippy.toml", "rustfmt.toml", "rust-toolchain.toml", "fuzz/Cargo.toml",
                  "apps/tui/src/main.rs"):
            self.assertEqual(self._detect(f)["rust"], "1", f)


if __name__ == "__main__":
    unittest.main()
