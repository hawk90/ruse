"""`tools/parity/divergence_issues.py` (#1583): every known-divergence entry cites an open issue, and the
check skips (exit 0) when `gh` or the network cannot answer."""
import importlib.util
import os
import subprocess
import tempfile
import unittest
from unittest import mock

from . import _support  # noqa: F401

PATH = os.path.join(_support.TOOLS, "parity", "divergence_issues.py")
SPEC = importlib.util.spec_from_file_location("divergence_issues", PATH)
di = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(di)


def listfile(body):
    f = tempfile.NamedTemporaryFile("w", suffix=".txt", delete=False, encoding="utf-8")
    f.write(body)
    f.close()
    return f.name


class EntriesTest(unittest.TestCase):
    def test_reads_the_cite_at_the_start_of_the_reason(self):
        body = "# header #900\n\na # #1622: reason\nb # reason (#728)\nc\n"
        self.assertEqual(di.entries(body), [(3, "a", 1622), (4, "b", None), (5, "c", None)])


class CheckTest(unittest.TestCase):
    def run_check(self, body, states):
        path = listfile(body)
        try:
            return di.check([path], state=lambda n: states.get(n))
        finally:
            os.unlink(path)

    def test_open_issues_pass(self):
        self.assertEqual(self.run_check("a # #1: r\nb # #1: r\n", {1: "OPEN"}), [])

    def test_closed_missing_and_uncited_entries_fail(self):
        problems = self.run_check("a # #1: r\nb # #2: r\nc # r\n", {1: "CLOSED"})
        self.assertEqual(len(problems), 3, problems)
        self.assertIn("`a` cites #1, which is closed", problems[0])
        self.assertIn("`b` cites #2, which does not exist", problems[1])
        self.assertIn("`c` cites no issue", problems[2])


class UnavailableTest(unittest.TestCase):
    def test_no_gh_skips(self):
        path = listfile("a # #1: r\n")
        try:
            with mock.patch.object(di.shutil, "which", return_value=None):
                self.assertEqual(di.main([path]), 0)
        finally:
            os.unlink(path)

    def test_network_error_skips(self):
        def offline(*_a, **_k):
            return subprocess.CompletedProcess([], 1, "", "error connecting to api.github.com")
        with mock.patch.object(di.shutil, "which", return_value="/usr/bin/gh"):
            with self.assertRaises(di.Unavailable):
                di.gh_state(1, run=offline)

    def test_unknown_issue_is_none(self):
        def missing(*_a, **_k):
            return subprocess.CompletedProcess([], 1, "", "GraphQL: Could not resolve to an issue")
        with mock.patch.object(di.shutil, "which", return_value="/usr/bin/gh"):
            self.assertIsNone(di.gh_state(99999, run=missing))


if __name__ == "__main__":
    unittest.main()
