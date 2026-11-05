import unittest

from . import _support  # noqa: F401
from rusekit.verify import run


CARGO_TEST_STDOUT = """
running 2 tests
test ok_one ... ok
test bad_one ... FAILED

failures:

---- bad_one stdout ----

thread 'bad_one' panicked at apps/tui/tests/x.rs:9:5:
no "LINE3 B" on screen: after 10.00s
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace


failures:
    bad_one

test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out
"""

CARGO_TEST_STDERR = """   Compiling ruse-tui v0.0.0
    Finished `test` profile [unoptimized + debuginfo] target(s) in 9.1s
     Running tests/x.rs (target/debug/deps/x-0123)
error: test failed, to rerun pass `-p ruse-tui --test x`
"""


class TestFailureExcerpt(unittest.TestCase):
    def test_a_failing_cargo_test_shows_its_panic_message(self):
        lines = run.failure_excerpt(CARGO_TEST_STDOUT, CARGO_TEST_STDERR)
        text = "\n".join(lines)
        self.assertIn("---- bad_one stdout ----", text)
        self.assertIn("thread 'bad_one' panicked at apps/tui/tests/x.rs:9:5:", text)
        self.assertIn('no "LINE3 B" on screen', text)
        self.assertIn("error: test failed", text)
        self.assertNotIn("test ok_one ... ok", text)

    def test_a_long_stderr_keeps_its_first_diagnostic_and_its_tail(self):
        err = ["    Checking ruse-tui"] * 3
        err += ["error: this loop never actually loops", "  --> apps/tui/src/a.rs:3:1"]
        err += [f"  | context {i}" for i in range(100)]
        err += [f"summary {i}" for i in range(8)]
        lines = run.failure_excerpt("", "\n".join(err))
        self.assertEqual(lines[0], "error: this loop never actually loops")
        self.assertIn("...", lines)
        self.assertEqual(lines[-8:], [f"summary {i}" for i in range(8)])
        self.assertLessEqual(len(lines), run.STDERR_HEAD_LINES + 1 + run.STDERR_TAIL_LINES)

    def test_a_short_stderr_is_shown_whole_once(self):
        lines = run.failure_excerpt("", "error: one\nsecond line")
        self.assertEqual(lines, ["error: one", "second line"])

    def test_the_test_report_is_bounded(self):
        out = "---- t stdout ----\n" + "\n".join(f"l{i}" for i in range(500)) + "\nfailures:\n"
        lines = run.failure_excerpt(out, "")
        self.assertEqual(len(lines), run.TEST_REPORT_LINES + 1)
        self.assertTrue(lines[-1].startswith("... (381 more lines"))


if __name__ == "__main__":
    unittest.main()
