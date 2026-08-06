import datetime
import unittest

from . import _support  # noqa: F401
from gov.waivers import validate  # noqa: E402

TODAY = datetime.date(2026, 8, 7)


def _full(**over):
    w = {"id": "W-1", "rule": "dependency-check", "reason": "x", "owner": "@a",
         "approved_by": "@b", "expires": "2099-01-01", "removal_spec": "RFC-1"}
    w.update(over)
    return w


class TestWaivers(unittest.TestCase):
    def v(self, ws):
        return validate(ws, today=TODAY)

    def test_empty_passes(self):
        e, _w, a = self.v([])
        self.assertEqual(e, [])
        self.assertEqual(a, [])

    def test_missing_fields_fail(self):
        e, _w, _a = self.v([{"id": "W", "rule": "x"}])
        self.assertTrue(any("missing" in x for x in e))

    def test_expired_fails(self):
        e, _w, a = self.v([_full(id="W-EXP", expires="2026-01-01")])
        self.assertTrue(any("EXPIRED" in x for x in e))
        self.assertEqual(a, [])

    def test_valid_future_is_active(self):
        e, _w, a = self.v([_full()])
        self.assertEqual(e, [])
        self.assertEqual(len(a), 1)

    def test_bad_date_fails(self):
        e, _w, _a = self.v([_full(expires="not-a-date")])
        self.assertTrue(any("ISO date" in x for x in e))

    def test_duplicate_id_fails(self):
        e, _w, _a = self.v([_full(), _full()])
        self.assertTrue(any("duplicate" in x for x in e))

    def test_near_expiry_warns_but_stays_active(self):
        e, w, a = self.v([_full(expires=str(TODAY + datetime.timedelta(days=7)))])
        self.assertEqual(e, [])
        self.assertEqual(len(a), 1)
        self.assertTrue(w)


if __name__ == "__main__":
    unittest.main()
