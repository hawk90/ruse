import datetime
import unittest

from . import _support  # noqa: F401
from rusekit.gov.quarantine import scan_source, validate  # noqa: E402

TODAY = datetime.date(2026, 11, 28)
TID = "crates/x/src/a.rs::slow_probe"


def _entry(**over):
    e = {"test": TID, "reason": "timing probe", "issue": 910, "owner": "maintainer", "expires": "2027-03-01"}
    e.update(over)
    return e


def _ignored(reason="timing probe (#910)", test=TID):
    return {"test": test, "reason": reason, "line": 3}


class TestScan(unittest.TestCase):
    def test_finds_reasoned_and_bare_ignores_with_fn_names(self):
        src = (
            "#[test]\n"
            '#[ignore = "timing probe (#910)"]\n'
            "#[allow(clippy::print_stdout)]\n"
            "fn slow_probe() {}\n"
            "    #[test]\n"
            "    #[ignore]\n"
            "    pub fn bare() {}\n"
        )
        got = scan_source("crates/x/src/a.rs", src)
        self.assertEqual([g["test"] for g in got], [TID, "crates/x/src/a.rs::bare"])
        self.assertEqual(got[0]["reason"], "timing probe (#910)")
        self.assertIsNone(got[1]["reason"])

    def test_doc_comment_mention_is_not_an_ignore(self):
        src = "/// the `#[ignore]` live smoke is the complement\n#[test]\nfn fast() {}\n"
        self.assertEqual(scan_source("a.rs", src), [])


class TestValidate(unittest.TestCase):
    def v(self, entries, ignored):
        return validate(entries, ignored, today=TODAY)

    def test_matched_entry_passes(self):
        e, w = self.v([_entry()], [_ignored()])
        self.assertEqual(e, [])
        self.assertEqual(w, [])

    def test_unlisted_ignore_fails(self):
        e, _w = self.v([], [_ignored()])
        self.assertTrue(any("no entry" in x for x in e))

    def test_stale_entry_fails(self):
        e, _w = self.v([_entry()], [])
        self.assertTrue(any("stale" in x for x in e))

    def test_expired_fails(self):
        e, _w = self.v([_entry(expires="2026-11-27")], [_ignored()])
        self.assertTrue(any("EXPIRED" in x for x in e))

    def test_expiry_day_itself_still_passes_with_a_warning(self):
        e, w = self.v([_entry(expires=str(TODAY))], [_ignored()])
        self.assertEqual(e, [])
        self.assertTrue(w)

    def test_bare_ignore_fails(self):
        e, _w = self.v([_entry()], [_ignored(reason=None)])
        self.assertTrue(any("bare" in x for x in e))

    def test_reason_must_cite_the_entry_issue(self):
        e, _w = self.v([_entry()], [_ignored(reason="timing probe (#9101)")])
        self.assertTrue(any("does not cite #910" in x for x in e))

    def test_missing_fields_fail(self):
        e, _w = self.v([{"test": TID, "issue": 910}], [_ignored()])
        self.assertTrue(any("missing" in x for x in e))

    def test_bad_date_and_bad_issue_fail(self):
        e, _w = self.v([_entry(expires="soon", issue="#910")], [_ignored()])
        self.assertTrue(any("ISO date" in x for x in e))
        self.assertTrue(any("not an issue number" in x for x in e))

    def test_duplicate_entry_fails(self):
        e, _w = self.v([_entry(), _entry()], [_ignored()])
        self.assertTrue(any("duplicate" in x for x in e))


if __name__ == "__main__":
    unittest.main()
