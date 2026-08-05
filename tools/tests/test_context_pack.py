import unittest

from . import _support  # noqa: F401
from ai.context_pack import build  # noqa: E402


class TestContextPack(unittest.TestCase):
    def test_always_include_is_applied(self):
        # context-profiles.yaml declares always_include: [PROJECT.md, CONTEXT.md] for every pack.
        _body, lock = build(None, ["CAP-EDIT-CORE"], 1, [])
        self.assertIn("spec/PROJECT.md", lock["sources"])
        self.assertIn("spec/CONTEXT.md", lock["sources"])

    def test_unknown_root_raises_not_silently_dropped(self):
        with self.assertRaises(ValueError):
            build(None, ["CAP-DOES-NOT-EXIST"], 1, [])

    def test_partial_unknown_still_raises(self):
        with self.assertRaises(ValueError):
            build(None, ["CAP-EDIT-CORE", "F-NOPE"], 1, [])

    def test_canonical_generated_marker(self):
        body, _lock = build(None, ["CAP-EDIT-CORE"], 1, [])
        self.assertIn("GENERATED FILE: DO NOT EDIT", body)

    def test_pack_lists_root_ids(self):
        body, _lock = build(None, ["CAP-EDIT-CORE"], 1, [])
        self.assertIn("CAP-EDIT-CORE", body)


if __name__ == "__main__":
    unittest.main()
