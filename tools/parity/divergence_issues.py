#!/usr/bin/env python3
"""Check that every known-divergence entry cites an OPEN issue (#1583).

Each entry in tests/parity/<editor>/known-divergence*.txt is `name # #<issue>: reason`. The Rust ratchet
(apps/tui/tests/parity_ratchet/mod.rs) checks offline that the cite is there; this checker asks GitHub,
through `gh`, that each cited issue is still open, so an entry does not outlive the issue that tracks it.

It needs `gh` and the network. Without them it prints SKIP and exits 0: a missing tool is not a failing
list. Exit 1 when a cited issue is closed or missing, or an entry cites none.

    python3 tools/parity/divergence_issues.py [FILE ...]
"""
import glob
import json
import os
import re
import shutil
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
DEFAULT_GLOB = os.path.join(ROOT, "tests", "parity", "*", "known-*divergences.txt")
CITE = re.compile(r"^#(\d+):")


class Unavailable(Exception):
    """`gh` or the network cannot answer; the check is skipped, not failed."""


def entries(body):
    """(line number, name, cited issue or None) for every entry of a known-divergences body."""
    out = []
    for i, line in enumerate(body.splitlines(), 1):
        name, _, reason = line.partition("#")
        name = name.strip()
        if not name:
            continue
        m = CITE.match(reason.strip())
        out.append((i, name, int(m.group(1)) if m else None))
    return out


def gh_state(number, run=subprocess.run):
    """The issue's state (`OPEN`, `CLOSED`), or None when it does not exist. Raises Unavailable."""
    if shutil.which("gh") is None:
        raise Unavailable("gh is not installed")
    try:
        proc = run(["gh", "issue", "view", str(number), "--json", "state"],
                   capture_output=True, text=True, timeout=30, cwd=ROOT)
    except (OSError, subprocess.TimeoutExpired) as e:
        raise Unavailable(f"gh failed: {e}") from e
    if proc.returncode == 0:
        return json.loads(proc.stdout)["state"]
    err = proc.stderr.lower()
    if "could not resolve to an issue" in err or "not found" in err:
        return None
    raise Unavailable(f"gh issue view {number}: {proc.stderr.strip() or proc.returncode}")


def check(files, state=gh_state):
    """Return the problems, one string each. Raises Unavailable when `state` cannot answer."""
    problems = []
    cache = {}
    for path in files:
        with open(path, encoding="utf-8") as f:
            body = f.read()
        rel = os.path.relpath(path, ROOT)
        for line, name, issue in entries(body):
            if issue is None:
                problems.append(f"{rel}:{line}: `{name}` cites no issue")
                continue
            if issue not in cache:
                cache[issue] = state(issue)
            st = cache[issue]
            if st is None:
                problems.append(f"{rel}:{line}: `{name}` cites #{issue}, which does not exist")
            elif st != "OPEN":
                problems.append(f"{rel}:{line}: `{name}` cites #{issue}, which is {st.lower()}; "
                                "repoint it to the open successor")
    return problems


def main(argv):
    files = argv or sorted(glob.glob(DEFAULT_GLOB))
    try:
        problems = check(files)
    except Unavailable as e:
        print(f"SKIP divergence issues: {e}")
        return 0
    for p in problems:
        print(f"FAIL {p}")
    if problems:
        return 1
    print(f"PASS every known-divergence entry cites an open issue ({len(files)} files)")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
