#!/usr/bin/env python3
"""gov quarantine — every `#[ignore]`d Rust test is an owned, expiring, tracked entry (#1320, OPS-003).

The testing policy says a muted test is tracked debt, not a silent skip: it carries an owner and an expiry
(docs/operations/testing-and-benchmarks.md "Test quarantine"). This checker holds the code to the list in
`tests/quarantine.yaml`:
  - every `#[ignore]` under crates/ and apps/ has an entry (keyed `<file>::<fn>`), and the attribute's reason
    string cites the entry's issue as `#<n>` — a bare `#[ignore]` FAILS;
  - every entry names an ignored test that still exists (a stale entry FAILS — un-ignore means remove it);
  - every entry carries test, reason, issue, owner, expires; an entry past `expires` FAILS (renew it with a
    new date and a reason, or fix the test). Within 14 days of expiry it WARNS.
It does not ask GitHub whether the issue is still open: the check stays offline and deterministic.

Auto-discovered into `ruse gov check`.
"""
from __future__ import annotations

import argparse
import datetime
import os
import re
import sys

import yaml

from rusekit import render, repo  # noqa: E402

LIST = "tests/quarantine.yaml"
ROOTS = ("crates", "apps")
REQUIRED = ("test", "reason", "issue", "owner", "expires")
WARN_DAYS = 14

IGNORE_RE = re.compile(r'^\s*#\[\s*ignore\s*(?:=\s*"((?:[^"\\]|\\.)*)"\s*)?\]')
FN_RE = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)")
FN_LOOKAHEAD = 8  # attribute lines between `#[ignore]` and its `fn`


def scan_source(rel: str, text: str) -> list[dict]:
    """The `#[ignore]` attributes in one Rust file: [{test, reason, line}] (reason None for a bare one)."""
    found: list[dict] = []
    lines = text.splitlines()
    for i, line in enumerate(lines):
        m = IGNORE_RE.match(line)
        if not m:
            continue
        name = None
        for nxt in lines[i + 1:i + 1 + FN_LOOKAHEAD]:
            f = FN_RE.match(nxt)
            if f:
                name = f.group(1)
                break
        found.append({"test": f"{rel}::{name or '<unknown fn>'}", "reason": m.group(1), "line": i + 1})
    return found


def scan_workspace() -> list[dict]:
    found: list[dict] = []
    for root in ROOTS:
        for dirpath, dirnames, filenames in os.walk(repo.path(root)):
            dirnames[:] = sorted(d for d in dirnames if d != "target" and not d.startswith("."))
            for fn in sorted(filenames):
                if fn.endswith(".rs"):
                    p = os.path.join(dirpath, fn)
                    with open(p, encoding="utf-8", errors="replace") as fh:
                        found.extend(scan_source(repo.rel(p), fh.read()))
    return found


def load() -> list[dict]:
    p = repo.path(LIST)
    if not os.path.isfile(p):
        return []
    with open(p, encoding="utf-8") as fh:
        return (yaml.safe_load(fh) or {}).get("quarantine") or []


def validate(entries: list[dict], ignored: list[dict], today: datetime.date | None = None):
    """Return (errors, warnings)."""
    today = today or datetime.date.today()
    errors: list[str] = []
    warns: list[str] = []
    by_test: dict[str, dict] = {}
    for e in entries:
        tid = e.get("test") or "<no test>"
        missing = [f for f in REQUIRED if not e.get(f)]
        if missing:
            errors.append(f"{tid}: missing required field(s) {missing}")
            continue
        if tid in by_test:
            errors.append(f"{tid}: duplicate quarantine entry")
            continue
        by_test[tid] = e
        if isinstance(e["issue"], bool) or not isinstance(e["issue"], int) or e["issue"] <= 0:
            errors.append(f"{tid}: issue '{e['issue']}' is not an issue number")
        exp = e["expires"]
        try:
            d = exp if isinstance(exp, datetime.date) else datetime.date.fromisoformat(str(exp))
        except ValueError:
            errors.append(f"{tid}: expires '{exp}' is not an ISO date (YYYY-MM-DD)")
            continue
        if d < today:
            errors.append(f"{tid}: EXPIRED {d} (owner {e['owner']}, #{e['issue']}) — fix the test, or renew "
                          f"the entry with a new date and reason")
        elif (d - today).days <= WARN_DAYS:
            warns.append(f"{tid}: quarantine expires in {(d - today).days}d ({d}) — #{e['issue']}")

    seen: set[str] = set()
    for ig in ignored:
        tid = ig["test"]
        seen.add(tid)
        e = by_test.get(tid)
        if e is None:
            errors.append(f"{tid} (line {ig['line']}): #[ignore] with no entry in {LIST}")
            continue
        if not ig["reason"]:
            errors.append(f"{tid} (line {ig['line']}): bare #[ignore] — give a reason citing #{e['issue']}")
        elif not re.search(rf"#{e['issue']}(?!\d)", ig["reason"]):
            errors.append(f"{tid} (line {ig['line']}): #[ignore] reason does not cite #{e['issue']}")
    for tid in by_test:
        if tid not in seen:
            errors.append(f"{tid}: stale entry — no such #[ignore]d test (remove it from {LIST})")
    return errors, warns


def main(argv: list[str]) -> int:
    argparse.ArgumentParser(prog="ruse gov quarantine").parse_args(argv)
    entries = load()
    ignored = scan_workspace()
    errors, warns = validate(entries, ignored)

    render.heading("gov quarantine")
    render.field("Ignored tests", str(len(ignored)))
    render.field("Entries", str(len(entries)))
    for wn in warns:
        render.warn(wn)
    for e in errors:
        render.fail(e)
    print()
    if errors:
        render.fail(f"gov quarantine: FAIL ({len(errors)} — every #[ignore] is an owned, expiring entry)")
        return 1
    render.ok(f"gov quarantine: PASS ({len(entries)} quarantined, none expired)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
