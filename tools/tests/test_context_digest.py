import unittest

from . import _support  # noqa: F401
from rusekit.gov import context_digest as cd  # noqa: E402

DECISIONS = """\
## D-001 — Transaction is the only path · decided
## D-005 — Save & recovery · decided (design) / open (tuning)
## D-012 — Multiple clients · open
## D-070 — Old choice · superseded by D-071
"""

INVARIANTS = """\
- **INV-TXN** — Every mutation goes through a Transaction.
- **INV-PLUGIN-NO-CORE** — A Plugin cannot mutate Core state.
- **INV-SCHED-1** — All background work is scheduled. RFCs list the INV-IDs they use.
"""


class TestParse(unittest.TestCase):
    def test_only_decided_decisions_are_required(self):
        self.assertEqual(cd.decided_decisions(DECISIONS), ["D-001", "D-005"])

    def test_invariant_definitions_not_mentions(self):
        self.assertEqual(cd.defined_invariants(INVARIANTS), ["INV-TXN", "INV-PLUGIN-NO-CORE", "INV-SCHED-1"])


class TestMissing(unittest.TestCase):
    def test_complete_digest_passes(self):
        digest = "(D-001, D-005) (INV-TXN) INV-PLUGIN-NO-CORE · INV-SCHED-1."
        self.assertEqual(cd.missing(digest, DECISIONS, INVARIANTS), ([], []))

    def test_missing_ids_are_reported(self):
        self.assertEqual(cd.missing("D-001 INV-TXN", DECISIONS, INVARIANTS),
                         (["D-005"], ["INV-PLUGIN-NO-CORE", "INV-SCHED-1"]))

    def test_glob_and_prefix_do_not_cite(self):
        # `INV-PLUGIN-*` names a family, not an ID; D-0010 and INV-TXN-X are different tokens.
        digest = "D-0010 D-005 INV-PLUGIN-* INV-TXN-X INV-SCHED-1"
        self.assertEqual(cd.missing(digest, DECISIONS, INVARIANTS),
                         (["D-001"], ["INV-TXN", "INV-PLUGIN-NO-CORE"]))


class TestRepo(unittest.TestCase):
    def test_real_digest_is_complete(self):
        self.assertEqual(cd.main([]), 0)


if __name__ == "__main__":
    unittest.main()
