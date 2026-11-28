import os
import tempfile
import unittest

from . import _support  # noqa: F401
from rusekit.gov import proptest_seeds as ps  # noqa: E402


def _touch(root: str, rel: str) -> None:
    p = os.path.join(root, *rel.split("/"))
    os.makedirs(os.path.dirname(p), exist_ok=True)
    with open(p, "w", encoding="utf-8") as fh:
        fh.write("")


class TestMapping(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.root = self._tmp.name
        _touch(self.root, "app/src/main.rs")
        _touch(self.root, "app/src/keys.rs")
        _touch(self.root, "app/src/persist/mod.rs")
        _touch(self.root, "app/tests/props.rs")

    def tearDown(self):
        self._tmp.cleanup()

    def test_source_parallel_maps_under_src(self):
        self.assertEqual(ps.source_candidates(self.root, "app/proptest-regressions/persist/mod.txt"),
                         ["app/src/persist/mod.rs"])

    def test_with_source_fallback_sits_next_to_the_source(self):
        self.assertEqual(ps.source_candidates(self.root, "app/tests/props.proptest-regressions"),
                         ["app/tests/props.rs"])

    def test_mapped_seeds_pass(self):
        _touch(self.root, "app/proptest-regressions/keys.txt")
        _touch(self.root, "app/proptest-regressions/persist/mod.txt")
        _touch(self.root, "app/tests/props.proptest-regressions")
        self.assertEqual(ps.orphans(self.root), [])

    def test_orphaned_seed_is_reported(self):
        # src/input.rs was split into src/input/*.rs: its seeds never replay again (#1585).
        _touch(self.root, "app/proptest-regressions/input.txt")
        _touch(self.root, "app/tests/gone.proptest-regressions")
        self.assertEqual(ps.orphans(self.root), [
            ("app/proptest-regressions/input.txt", ["app/src/input.rs"]),
            ("app/tests/gone.proptest-regressions", ["app/tests/gone.rs"]),
        ])

    def test_no_crate_root_means_no_source(self):
        # Without a lib.rs/main.rs beside the sibling dir, proptest never writes there.
        _touch(self.root, "lone/proptest-regressions/x.txt")
        _touch(self.root, "lone/src/x.rs")
        self.assertEqual(ps.orphans(self.root), [("lone/proptest-regressions/x.txt", [])])

    def test_build_dirs_are_not_scanned(self):
        _touch(self.root, "target/proptest-regressions/stale.txt")
        self.assertEqual(ps.seed_files(self.root), [])


class TestRepo(unittest.TestCase):
    def test_real_repo_has_no_orphaned_seeds(self):
        self.assertEqual(ps.main([]), 0)


if __name__ == "__main__":
    unittest.main()
