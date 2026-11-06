"""The path labeler (.github/labeler.yml) only applies labels that exist, from globs that match real files (#930)."""
import os
import re
import unittest

import yaml

from . import _support  # noqa: F401  (sys.path side effect)
from rusekit import repo  # noqa: E402


def glob_re(glob):
    """A minimatch glob as a full-path regex: `**` spans directories, `*` and `?` stay inside one."""
    out, i = "", 0
    while i < len(glob):
        if glob.startswith("**/", i):
            out, i = out + "(?:.*/)?", i + 3
        elif glob.startswith("**", i):
            out, i = out + ".*", i + 2
        elif glob[i] == "*":
            out, i = out + "[^/]*", i + 1
        elif glob[i] == "?":
            out, i = out + "[^/]", i + 1
        else:
            out, i = out + re.escape(glob[i]), i + 1
    return re.compile(out + r"\Z")


def labeler_globs(rules):
    """(label, glob) for every changed-files glob in a labeler v5+ config."""
    for label, matchers in rules.items():
        for m in matchers:
            for cf in (m.get("changed-files") or []):
                for key, globs in cf.items():
                    for g in ([globs] if isinstance(globs, str) else globs):
                        yield label, key, g


class TestLabeler(unittest.TestCase):
    def setUp(self):
        root = repo.path(".github")
        with open(os.path.join(root, "labeler.yml")) as f:
            self.rules = yaml.safe_load(f)
        with open(os.path.join(root, "labels.yml")) as f:
            self.labels = {l["name"] for l in yaml.safe_load(f)}

    def test_every_label_is_defined(self):
        for label in self.rules:
            self.assertIn(label, self.labels, f"labeler applies {label}, which .github/labels.yml does not define")

    def test_every_glob_matches_a_tracked_file(self):
        code, out = repo._git("ls-files")
        if code != 0:
            self.skipTest("not a git checkout")
        files = out.splitlines()
        seen = 0
        for label, key, g in labeler_globs(self.rules):
            self.assertIn(key, ("any-glob-to-any-file", "any-glob-to-all-files",
                                "all-globs-to-any-file", "all-globs-to-all-files"), label)
            self.assertNotIn("{", g, f"{label}: brace globs are not checked here; list each path")
            rx = glob_re(g)
            self.assertTrue(any(rx.match(f) for f in files), f"{label}: glob {g!r} matches no tracked file")
            seen += 1
        self.assertGreater(seen, 0)

    def test_glob_re(self):
        self.assertTrue(glob_re("crates/core/**").match("crates/core/src/lib.rs"))
        self.assertTrue(glob_re("apps/tui/src/ui/**").match("apps/tui/src/ui/render/x.rs"))
        self.assertFalse(glob_re("apps/tui/src/*.rs").match("apps/tui/src/ui/mod.rs"))
        self.assertFalse(glob_re("crates/render-model/**").match("crates/core/src/lib.rs"))


if __name__ == "__main__":
    unittest.main()


class TestLabelColors(unittest.TestCase):
    def test_every_color_is_a_quoted_string(self):
        # EndBug/label-sync parses YAML 1.2, where an unquoted `5319e7` is a number;
        # PyYAML (YAML 1.1) reads it as a string, so loading the file cannot catch it.
        path = os.path.join(repo.ROOT, ".github", "labels.yml")
        with open(path, encoding="utf-8") as f:
            for n, line in enumerate(f, 1):
                m = re.search(r"\bcolor:\s*([^,}\s]+)", line)
                if m:
                    self.assertRegex(m.group(1), r'^"[0-9a-fA-F]{6}"$', f"labels.yml:{n} color must be a quoted hex string")
