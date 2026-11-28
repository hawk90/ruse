import unittest

from . import _support  # noqa: F401
from rusekit.docs.check import (  # noqa: E402
    defines_item, headings_slugs, slugify, stale_source_paths,
)


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

    def test_a_regex_pattern_is_not_a_path(self):
        # spec/change-kinds.yaml writes `'^apps/tui/src/remote/protocol\.rs$'`
        self.assertEqual(self.stale(r"pattern: '^apps/tui/src/remote/protocol\.rs$'"), [])

    def test_a_yaml_evidence_list_is_checked_like_prose(self):
        text = "    evidence:\n      - apps/tui/src/app/session.rs::run\n      - apps/tui/src/gone.rs::x\n"
        self.assertEqual(self.stale(text), [(3, "apps/tui/src/gone.rs")])


class TestDocsCheckItemRefs(unittest.TestCase):
    SOURCE = {
        "apps/tui/src/app/session.rs": "pub struct Session;\n#[test]\nfn runs_to_exit() {}\n"
                                       "pub(crate) const MAX: usize = 1;\nmod tests {}\n",
    }

    def stale(self, text):
        live = set(self.SOURCE) | {"crates/core/src/editor/ex_cmd", "docs/x.md"}
        return stale_source_paths(text, live.__contains__,
                                  lambda path, name: defines_item(self.SOURCE[path], name))

    def test_a_declared_item_passes(self):
        text = ("apps/tui/src/app/session.rs::Session apps/tui/src/app/session.rs::runs_to_exit "
                "apps/tui/src/app/session.rs::MAX apps/tui/src/app/session.rs::tests")
        self.assertEqual(self.stale(text), [])

    def test_a_missing_item_is_reported_as_path_and_name(self):
        text = "- apps/tui/src/app/session.rs::torn_header_tail_is_discarded"
        self.assertEqual(self.stale(text),
                         [(1, "apps/tui/src/app/session.rs::torn_header_tail_is_discarded")])

    def test_only_the_first_segment_is_checked(self):
        self.assertEqual(self.stale("apps/tui/src/app/session.rs::Session::not_checked"), [])

    def test_a_line_suffix_or_a_directory_is_not_an_item_reference(self):
        self.assertEqual(self.stale("apps/tui/src/app/session.rs:12 crates/core/src/editor/ex_cmd::x"), [])

    def test_a_name_must_be_declared_not_merely_mentioned(self):
        self.assertFalse(defines_item("let x = Session::new(); // fn_like", "fn_like"))
        self.assertFalse(defines_item("fn runs_to_exit_twice() {}", "runs_to_exit"))
        self.assertTrue(defines_item("macro_rules! oracle { () => {} }", "oracle"))


if __name__ == "__main__":
    unittest.main()
