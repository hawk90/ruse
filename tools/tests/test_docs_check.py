import unittest

from . import _support  # noqa: F401
from docs.check import slugify, headings_slugs  # noqa: E402


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


if __name__ == "__main__":
    unittest.main()
