import unittest

from . import _support  # noqa: F401
from rusekit.gov.constitution import load, validate  # noqa: E402


class TestConstitution(unittest.TestCase):
    def test_repo_constitution_is_valid(self):
        errors, _w, _cov = validate(load())
        self.assertEqual(errors, [], f"constitution invalid: {errors}")

    def test_editor_not_ide_article_present(self):
        ids = {a.get("id") for a in load()}
        self.assertIn("CON-EDITOR-NOT-IDE", ids)
        self.assertIn("CON-CORE-INDEPENDENT", ids)

    def test_core_independence_is_machine_enforced(self):
        a = next(a for a in load() if a["id"] == "CON-CORE-INDEPENDENT")
        self.assertEqual(a["enforcement"], "check")
        self.assertTrue(a.get("check"))

    def test_bad_id_fails(self):
        e, _w, _c = validate([{"id": "editor-not-ide", "statement": "x", "enforcement": "review"}])
        self.assertTrue(any("CON-" in x for x in e))

    def test_check_without_checker_fails(self):
        e, _w, _c = validate([{"id": "CON-X", "statement": "x", "enforcement": "check"}])
        self.assertTrue(any("no `check:`" in x for x in e))

    def test_bad_enforcement_fails(self):
        e, _w, _c = validate([{"id": "CON-X", "statement": "x", "enforcement": "maybe"}])
        self.assertTrue(any("enforcement must be" in x for x in e))

    def test_coverage_counts(self):
        _e, _w, (checked, review) = validate(load())
        self.assertGreater(checked, 0)
        self.assertGreater(review, 0)


if __name__ == "__main__":
    unittest.main()
