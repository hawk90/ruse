import unittest

from . import _support  # noqa: F401
from rusekit.gov.fitness import load, validate  # noqa: E402


class TestFitness(unittest.TestCase):
    def test_repo_fitness_is_valid(self):
        errors, _w, _cov = validate(load())
        self.assertEqual(errors, [], f"fitness invalid: {errors}")

    def test_has_live_and_planned(self):
        _e, _w, (live, planned) = validate(load())
        self.assertGreater(live, 0)
        self.assertGreater(planned, 0)

    def test_live_functions_name_a_checker(self):
        for f in load():
            if f.get("status") == "live":
                self.assertTrue(f.get("check"), f"{f['id']} live without a checker")

    def test_bad_operator_fails(self):
        e, _w, _c = validate([{"id": "FIT-X", "metric": "m", "operator": "roughly",
                               "threshold": 1, "kind": "atomic", "status": "planned"}])
        self.assertTrue(any("operator must be" in x for x in e))

    def test_non_numeric_threshold_fails(self):
        e, _w, _c = validate([{"id": "FIT-X", "metric": "m", "operator": "equals",
                               "threshold": "zero", "kind": "atomic", "status": "planned"}])
        self.assertTrue(any("numeric" in x for x in e))

    def test_live_without_checker_fails(self):
        e, _w, _c = validate([{"id": "FIT-X", "metric": "m", "operator": "equals",
                               "threshold": 0, "kind": "holistic", "status": "live"}])
        self.assertTrue(any("no enforcing" in x for x in e))

    def test_bad_id_fails(self):
        e, _w, _c = validate([{"id": "cycles", "metric": "m", "operator": "equals",
                               "threshold": 0, "kind": "holistic", "status": "planned"}])
        self.assertTrue(any("FIT-" in x for x in e))


if __name__ == "__main__":
    unittest.main()
