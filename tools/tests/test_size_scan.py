import contextlib
import io
import os
import tempfile
import unittest
from unittest import mock

from . import _support  # noqa: F401
from rusekit.arch import size_scan as ss  # noqa: E402
from rusekit.gov import ratchet  # noqa: E402


def fn(name, body_lines, indent=""):
    """A Rust fn whose span (fn line .. closing brace) is body_lines + 2 lines."""
    body = "".join(f"{indent}    let _x{i} = {i};\n" for i in range(body_lines))
    return f"{indent}fn {name}() {{\n{body}{indent}}}\n"


class TestMask(unittest.TestCase):
    def assertBraceFree(self, src):
        masked = ss.mask(src)
        self.assertEqual(masked.count("\n"), src.count("\n"))
        self.assertNotIn("{", masked)
        self.assertNotIn("}", masked)

    def test_line_and_block_comments(self):
        self.assertBraceFree("// {\n/* } /* nested { */ } */\n")

    def test_strings_and_byte_strings(self):
        self.assertBraceFree('let a = "{ \\" }";\nlet b = b"}";\n')

    def test_raw_strings(self):
        self.assertBraceFree('let a = r"{";\nlet b = r#"}" { "#;\nlet c = br##"{"#}"##;\n')

    def test_char_literals(self):
        self.assertBraceFree("let a = '{';\nlet b = '}';\nlet c = '\\u{7b}';\n")

    def test_lifetime_is_not_a_char(self):
        self.assertIn("{", ss.mask("fn f<'a>(x: &'a str) {\n}\n"))

    def test_identifier_ending_in_r_is_not_a_raw_string(self):
        self.assertEqual(ss.mask('for"x"'), "for   ")

    def test_masked_brace_does_not_end_a_fn(self):
        src = 'fn f() {\n    let s = "}";\n    let c = \'}\';\n    // }\n}\n'
        self.assertEqual(ss.scan_source(src)[1], [(1, "f", 5)])


class TestScanSource(unittest.TestCase):
    def test_fn_length_is_fn_line_to_closing_brace(self):
        self.assertEqual(ss.scan_source(fn("f", 3))[1], [(1, "f", 5)])

    def test_bodyless_fn_is_skipped(self):
        src = "trait T {\n    fn f(&self);\n}\n"
        self.assertEqual(ss.scan_source(src)[1], [])

    def test_impl_qualifies_the_name(self):
        src = "impl Foo {\n" + fn("new", 1, "    ") + "}\nimpl fmt::Display for Bar {\n" + \
              fn("fmt", 1, "    ") + "}\n"
        self.assertEqual([n for _, n, _ in ss.scan_source(src)[1]], ["Foo::new", "Bar::fmt"])

    def test_cfg_test_module_is_not_counted(self):
        src = fn("f", 1) + "#[cfg(test)]\n#[allow(dead_code)]\nmod tests {\n" + fn("t", 200, "    ") + "}\n"
        lines, fns = ss.scan_source(src)
        self.assertEqual(lines, 3)
        self.assertEqual([n for _, n, _ in fns], ["f"])


class TestPaths(unittest.TestCase):
    def test_test_paths(self):
        for rel in ("apps/tui/tests/x.rs", "crates/core/benches/b.rs", "crates/core/src/a/tests.rs",
                    "crates/core/src/test.rs", "apps/tui/src/feed_tests.rs"):
            self.assertTrue(ss.is_test_path(rel), rel)
        for rel in ("crates/core/src/testing.rs", "apps/tui/src/tests_util/x.rs", "crates/core/src/lib.rs"):
            self.assertFalse(ss.is_test_path(rel), rel)

    def test_scan_skips_tests_and_exemptions(self):
        with tempfile.TemporaryDirectory() as root:
            def put(rel, text):
                os.makedirs(os.path.dirname(os.path.join(root, rel)), exist_ok=True)
                with open(os.path.join(root, rel), "w", encoding="utf-8") as fh:
                    fh.write(text)
            big = fn("long", 160)
            put("crates/core/src/a.rs", big)
            put("crates/core/src/a_tests.rs", big)
            put("crates/core/tests/it.rs", big)
            put("crates/core/src/case_tables.rs", big)
            put("apps/tui/src/input/emacs.rs", "impl EmacsProfile {\n" + fn("new", 160, "    ") + "}\n")
            put("apps/tui/src/b.rs", "\n" * 1001)
            r = ss.scan(root)
        self.assertEqual(r["fn_over_150"], [("crates/core/src/a.rs", 1, "long", 162)])
        self.assertEqual(r["file_over_1000"], [("apps/tui/src/b.rs", 1, "", 1001)])


class TestRatchetGate(unittest.TestCase):
    def test_a_risen_size_bucket_lists_its_offenders(self):
        rows = {"fn_over_150": [("x.rs", 3, "f", 200)], "file_over_1000": []}
        real_load = ratchet._load
        head = real_load(ratchet.BASELINE)
        head = {**head, "buckets": {**head["buckets"], "fn_over_150": 0}}
        out = io.StringIO()
        with mock.patch.object(ss, "scan", return_value=rows), \
                mock.patch.object(ratchet, "_load",
                                  side_effect=lambda p: head if p == ratchet.BASELINE else real_load(p)), \
                mock.patch.object(ratchet, "base_baseline", return_value=({}, "test")), \
                contextlib.redirect_stdout(out):
            code = ratchet.main([])
        self.assertEqual(code, 1)
        self.assertIn("fn_over_150 rose to 1, above ceiling 0", out.getvalue())
        self.assertIn("fn_over_150: x.rs:3 f 200", out.getvalue())


if __name__ == "__main__":
    unittest.main()
