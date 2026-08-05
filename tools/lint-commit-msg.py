#!/usr/bin/env python3
"""lint-commit-msg — enforce the ruse commit-subject convention (github-workflow.md §8).

Run by the Lefthook `commit-msg` hook. Validates only the subject line:
  <type>(<scope>)?!?: <subject>
where <type> is one of the ruse subset. The allowed types are read from the §8 table so
there is one home for them (falls back to the known set if the doc is unreadable).

It cannot judge "why, not the filename" — that stays review's job — but it catches the
mechanical mistakes: unknown/missing type, empty subject, trailing period, over-long line.
"""
from __future__ import annotations

import os
import re
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
DOC = os.path.join(ROOT, "docs", "operations", "github-workflow.md")
FALLBACK = ["spec", "rfc", "feat", "fix", "refactor", "test", "bench", "docs", "build", "chore"]
MAX_SUBJECT = 72

# git-generated / tooling subjects we must not reject
SKIP_PREFIXES = ("Merge ", "Revert ", "fixup! ", "squash! ", "amend! ", "Reapply ")


def allowed_types() -> list[str]:
    """Parse the §8 table rows: `| \\`type\\` | meaning |`."""
    try:
        text = open(DOC, encoding="utf-8").read()
    except OSError:
        return FALLBACK
    found = re.findall(r"^\|\s*`([a-z]+)`\s*\|", text, re.M)
    return found or FALLBACK


def subject_of(msg: str) -> str | None:
    for line in msg.splitlines():
        s = line.rstrip()
        if not s.strip() or s.lstrip().startswith("#"):
            continue
        return s
    return None


def main(argv: list[str]) -> int:
    if not argv:
        print("lint-commit-msg: no message file given", file=sys.stderr)
        return 0  # don't block on a harness mistake
    try:
        msg = open(argv[0], encoding="utf-8").read()
    except OSError as e:
        print(f"lint-commit-msg: cannot read {argv[0]}: {e}", file=sys.stderr)
        return 0

    subject = subject_of(msg)
    if subject is None or subject.startswith(SKIP_PREFIXES):
        return 0

    types = allowed_types()
    pattern = re.compile(r"^(?P<type>[a-z]+)(?:\([\w.\-/ ]+\))?!?: (?P<subject>.+)$")
    m = pattern.match(subject)

    errors = []
    if not m:
        errors.append("subject must be  <type>(<scope>)?: <description>")
    else:
        if m.group("type") not in types:
            errors.append(f"unknown type '{m.group('type')}' — use one of: {' '.join(types)}")
        desc = m.group("subject").strip()
        if not desc:
            errors.append("empty description after ':'")
        if desc.endswith("."):
            errors.append("drop the trailing period")
    if len(subject) > MAX_SUBJECT:
        errors.append(f"subject is {len(subject)} chars; keep it <= {MAX_SUBJECT}")

    if not errors:
        return 0

    print("\n✗ commit message rejected (docs/operations/github-workflow.md §8)\n")
    print(f"  subject: {subject}\n")
    for e in errors:
        print(f"  - {e}")
    print("\n  types:  " + " ".join(types))
    print("  good:   fix(core): reject transactions based on stale revisions")
    print("  good:   docs: document the change workflow\n")
    return 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
