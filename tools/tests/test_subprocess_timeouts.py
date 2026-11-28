"""Subprocess timeouts in the bench, phase_sync and parity fetch tools (#1325, RA-TOOL-017): a hung child is
killed after the module's timeout and the tool fails with a message naming the command and the timeout.

Each test puts a fake `cargo` / `gh` that sleeps on PATH (or runs `sleep` directly), shrinks the timeout,
and asserts the call returns well before the sleep would end."""
import contextlib
import importlib.util
import io
import os
import stat
import tempfile
import time
import unittest
from unittest import mock

from . import _support  # noqa: F401

SLEEP = 30      # the hung child's runtime; a passing test returns long before this
TIMEOUT = 0.5   # the shrunken timeout under test
BOUND = 10      # elapsed seconds that would mean the child was not killed


def _load(name: str, *rel: str):
    spec = importlib.util.spec_from_file_location(name, os.path.join(_support.TOOLS, *rel))
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def _fake_bin(directory: str, name: str) -> None:
    """A `name` executable that execs `sleep`, so the sleeper is the direct child the timeout kills."""
    path = os.path.join(directory, name)
    with open(path, "w", encoding="utf-8") as fh:
        fh.write(f"#!/bin/sh\nexec sleep {SLEEP}\n")
    os.chmod(path, os.stat(path).st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)


@contextlib.contextmanager
def _hanging(name: str):
    with tempfile.TemporaryDirectory() as d:
        _fake_bin(d, name)
        path = d + os.pathsep + os.environ.get("PATH", "")
        with mock.patch.dict(os.environ, {"PATH": path}):
            yield


class BenchTimeoutTest(unittest.TestCase):
    def test_hung_cargo_bench_is_killed(self):
        from rusekit import bench
        out = io.StringIO()
        with _hanging("cargo"), contextlib.redirect_stdout(out), contextlib.redirect_stderr(out):
            start = time.monotonic()
            rc = bench.main(["--timeout", str(TIMEOUT)])
            elapsed = time.monotonic() - start
        self.assertEqual(rc, 124)
        self.assertLess(elapsed, BOUND)
        self.assertIn("cargo bench", out.getvalue())
        self.assertIn(f"timed out after {TIMEOUT:g}s", out.getvalue())

    def test_default_timeout_is_set(self):
        from rusekit import bench
        self.assertGreater(bench.TIMEOUT_SECS, 0)


class PhaseSyncTimeoutTest(unittest.TestCase):
    def test_hung_gh_is_killed(self):
        phase_sync = _load("phase_sync_under_test", "phase_sync.py")
        with _hanging("gh"), mock.patch.object(phase_sync, "GH_TIMEOUT_SECS", TIMEOUT):
            start = time.monotonic()
            with self.assertRaises(SystemExit) as cm:
                phase_sync._gh(["api", "repos/{owner}/{repo}/milestones"])
            elapsed = time.monotonic() - start
        self.assertLess(elapsed, BOUND)
        msg = str(cm.exception.code)  # a str code: SystemExit prints it and exits 1
        self.assertIn("gh api repos/{owner}/{repo}/milestones", msg)
        self.assertIn(f"timed out after {TIMEOUT:g}s", msg)


class FetchTimeoutTest(unittest.TestCase):
    def test_hung_git_is_killed(self):
        fetch = _load("parity_fetch_under_test", "parity", "fetch.py")
        out = io.StringIO()
        with tempfile.TemporaryDirectory() as d, \
                mock.patch.object(fetch, "GIT_TIMEOUT_SECS", TIMEOUT), \
                contextlib.redirect_stdout(out), contextlib.redirect_stderr(out):
            start = time.monotonic()
            code, msg = fetch._run(["sleep", str(SLEEP)], d)
            elapsed = time.monotonic() - start
        self.assertEqual(code, 124)
        self.assertLess(elapsed, BOUND)
        self.assertIn(f"`sleep {SLEEP}`", msg)
        self.assertIn(f"timed out after {TIMEOUT:g}s", msg)

    def test_timeout_fails_the_fetch(self):
        """A hung `git fetch` surfaces as a failed upstream (non-zero exit), not a hang."""
        fetch = _load("parity_fetch_under_test2", "parity", "fetch.py")
        want = "0" * 40
        with tempfile.TemporaryDirectory() as d:
            os.makedirs(os.path.join(d, "x", ".git"))
            out = io.StringIO()
            with _hanging("git"), mock.patch.object(fetch, "GIT_TIMEOUT_SECS", TIMEOUT), \
                    mock.patch.object(fetch, "cache_dir", lambda editor: os.path.join(d, editor)), \
                    contextlib.redirect_stdout(out), contextlib.redirect_stderr(out):
                ok, msg = fetch._fetch_locked("x", {"revision": want, "repo": "unused"})
        self.assertFalse(ok)
        self.assertIn("timed out", msg)


if __name__ == "__main__":
    unittest.main()
