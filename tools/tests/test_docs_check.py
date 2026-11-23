import unittest

from . import _support  # noqa: F401
from rusekit.docs.check import slugify, headings_slugs, stale_source_paths  # noqa: E402


class TestDocsCheckSlugs(unittest.TestCase):
    def test_simple_slug(self):
        self.assertEqual(slugify("Simple Title"), "simple-title")

    def test_em_dash_keeps_double_hyphen_like_github(self):
        # "TITLE — Sub" -> em-dash removed, its two flanking spaces become "--"
        self.assertEqual(slugify("VIM-MARK — Marks, Jumplist"),
                         "vim-mark--marks-jumplist")

    def test_explicit_id_is_honored(self):
        slugs = headings_slugs("### Input-source boundary {#input-source-boundary}")
        self.assertIn("input-source-boundary", slugs)

    def test_duplicate_headings_get_suffix(self):
        slugs = headings_slugs("## Notes\n## Notes\n")
        self.assertIn("notes", slugs)
        self.assertIn("notes-1", slugs)

    def test_code_fence_headings_ignored(self):
        text = "# Real\n```\n# not a heading\n```\n"
        slugs = headings_slugs(text)
        self.assertIn("real", slugs)
        self.assertNotIn("not-a-heading", slugs)


class TestDocsCheckSourcePaths(unittest.TestCase):
    LIVE = {"apps/tui/src/app/session.rs", "crates/core/src/editor/ex_cmd"}

    def stale(self, text):
        return stale_source_paths(text, self.LIVE.__contains__)

    def test_a_moved_file_is_reported_with_its_line(self):
        text = "ok\nthe codec (`crates/core/src/editor/ex_cmd.rs`) and\n"
        self.assertEqual(self.stale(text), [(2, "crates/core/src/editor/ex_cmd.rs")])

    def test_a_live_file_or_directory_passes_with_a_line_or_item_suffix(self):
        text = "`apps/tui/src/app/session.rs:209`, apps/tui/src/app/session.rs::run, crates/core/src/editor/ex_cmd/"
        self.assertEqual(self.stale(text), [])

    def test_globs_sets_and_placeholders_are_not_paths(self):
        text = "'crates/core/**', crates/core/src/{workspace,editor}.rs, apps/<name>/"
        self.assertEqual(self.stale(text), [])

    def test_a_path_inside_another_path_is_not_a_source_path(self):
        self.assertEqual(self.stale("vendor/crates/foo.rs, x.apps/y"), [])


if __name__ == "__main__":
    unittest.main()
