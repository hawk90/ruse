"""The nvim oracle's fixture inputs are data files (#1590): `tools/parity/oracle.py` loads
tools/parity/fixtures/nvim/*.jsonl in a fixed order and refuses a file it would not know where to place."""
import importlib.util
import json
import os
import tempfile
import unittest
from pathlib import Path

from . import _support  # noqa: F401

PATH = os.path.join(_support.TOOLS, "parity", "oracle.py")
SPEC = importlib.util.spec_from_file_location("oracle", PATH)
oracle = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(oracle)


class OracleFixturesTest(unittest.TestCase):
    def _dir(self, files: dict[str, str]) -> Path:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        for name, text in files.items():
            (Path(tmp.name) / name).write_text(text, encoding="utf-8")
        return Path(tmp.name)

    def test_load_order_is_legacy_by_name_then_issues_by_number(self):
        d = self._dir({
            "i1600.jsonl": '{"name": "c", "lines": [""], "keys": "x"}\n',
            "01-legacy.jsonl": '{"name": "b", "lines": [""], "keys": "x"}\n',
            "i999.jsonl": '{"name": "z", "lines": [""], "keys": "x"}\n',
            "00-legacy.jsonl": '# a comment\n\n{"name": "a", "lines": [""], "keys": "x"}\n',
        })
        names = [f["name"] for f in oracle.load_fixtures(d)]
        self.assertEqual(names, ["a", "b", "z", "c"])

    def test_strings_round_trip_exactly(self):
        spec = {"name": "t", "lines": ["a\t \u0085  ", "\xff"], "keys": "\x16j\x1b", "bytes": True}
        d = self._dir({"i1.jsonl": json.dumps(spec) + "\n"})
        self.assertEqual(oracle.load_fixtures(d), [spec])

    def test_unknown_file_name_is_refused(self):
        d = self._dir({"fixtures.json": "{}\n"})
        with self.assertRaises(oracle.OracleError):
            oracle.load_fixtures(d)

    def test_bad_line_names_file_and_line(self):
        d = self._dir({"i7.jsonl": '# ok\n{"name": "x", "lines": []}\n'})
        with self.assertRaisesRegex(oracle.OracleError, r"i7\.jsonl:2"):
            oracle.load_fixtures(d)

    def test_committed_fixtures_load_with_unique_names(self):
        names = [f["name"] for f in oracle.FIXTURES]
        self.assertGreater(len(names), 5000)
        self.assertEqual(len(names), len(set(names)))


if __name__ == "__main__":
    unittest.main()
