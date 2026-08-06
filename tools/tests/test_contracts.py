import unittest

from . import _support  # noqa: F401
from rusekit.gov.contracts import load, validate  # noqa: E402


class TestContracts(unittest.TestCase):
    def test_repo_contracts_are_valid(self):
        errors, _w, _cov = validate(load())
        self.assertEqual(errors, [], f"contracts invalid: {errors}")

    def test_expected_contracts_present(self):
        ids = {c.get("id") for _f, c in load()}
        self.assertIn("CONTRACT-PLUGIN", ids)
        self.assertIn("CONTRACT-REMOTE", ids)
        self.assertIn("CONTRACT-PERSISTENCE", ids)

    def test_all_declared_pre_impl(self):
        _e, _w, (declared, active) = validate(load())
        self.assertGreater(declared, 0)

    def test_bad_kind_fails(self):
        e, _w, _c = validate([("x.yaml", {"id": "CONTRACT-X", "title": "t", "kind": "widget",
                                          "compatibility": "backward", "status": "declared",
                                          "guarantees": ["g"]})])
        self.assertTrue(any("kind must be" in x for x in e))

    def test_missing_guarantee_fails(self):
        e, _w, _c = validate([("x.yaml", {"id": "CONTRACT-X", "title": "t", "kind": "api",
                                          "compatibility": "backward", "status": "declared",
                                          "guarantees": []})])
        self.assertTrue(any("guarantee" in x for x in e))

    def test_active_without_tests_fails(self):
        e, _w, _c = validate([("x.yaml", {"id": "CONTRACT-X", "title": "t", "kind": "api",
                                          "compatibility": "backward", "status": "active",
                                          "guarantees": ["g"],
                                          "verified_by": ["tests/contract/does-not-exist"]})])
        self.assertTrue(any("does not exist" in x for x in e))

    def test_bad_id_fails(self):
        e, _w, _c = validate([("x.yaml", {"id": "plugin", "title": "t", "kind": "api",
                                          "compatibility": "backward", "status": "declared",
                                          "guarantees": ["g"]})])
        self.assertTrue(any("CONTRACT-" in x for x in e))


if __name__ == "__main__":
    unittest.main()
