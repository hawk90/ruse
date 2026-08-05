"""plan validate — check that an implementation plan covers every required perspective.

It cannot judge whether a plan is *correct* — only whether a section was skipped. Missing
a required heading, or leaving one empty/at its template placeholder, is a finding. A human
still approves the plan's substance before Execute.
"""
from __future__ import annotations

import argparse
import os
import re
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from rusekit import repo, render  # noqa: E402

REQUIRED = [
    "Goal", "Non-goals", "Assumptions", "Affected spec IDs", "Expected files",
    "Invariants to preserve", "Implementation steps", "Tests", "Failure handling",
    "Compatibility", "Rollback", "Open questions",
]

PLACEHOLDER = re.compile(r"^\s*(<[^>]+>|_.*_|)\s*$")


def _sections(text: str) -> dict[str, str]:
    out: dict[str, str] = {}
    cur = None
    buf: list[str] = []
    for line in text.splitlines():
        m = re.match(r"^##\s+(.*\S)\s*$", line)
        if m:
            if cur is not None:
                out[cur] = "\n".join(buf).strip()
            cur = m.group(1).strip()
            buf = []
        elif cur is not None:
            buf.append(line)
    if cur is not None:
        out[cur] = "\n".join(buf).strip()
    return out


def _is_empty(body: str) -> bool:
    lines = [ln for ln in body.splitlines() if ln.strip() and not ln.strip().startswith("<!--")]
    if not lines:
        return True
    # all lines are placeholders (<...>, _italic hints_) or empty bullets
    for ln in lines:
        s = re.sub(r"^\s*[-*\d.]+\s*", "", ln)
        if not PLACEHOLDER.match(s):
            return False
    return True


def main(argv: list[str]) -> int:
    ap = argparse.ArgumentParser(prog="ruse plan validate")
    ap.add_argument("plan", nargs="?", help="path to plan.md (default: active issue's plan)")
    ap.add_argument("--issue")
    args = ap.parse_args(argv)

    path = args.plan
    if not path:
        issue = args.issue or repo.active_issue()
        if not issue:
            render.fail("no plan path and no active workspace")
            return 1
        path = os.path.join(repo.work_dir(issue), "plan.md")
    if not os.path.isfile(path):
        render.fail(f"plan not found: {path}")
        return 1

    text = open(path, encoding="utf-8").read()
    secs = _sections(text)

    missing, empty, present = [], [], []
    for req in REQUIRED:
        if req not in secs:
            missing.append(req)
        elif _is_empty(secs[req]):
            empty.append(req)
        else:
            present.append(req)

    render.heading("plan validate")
    render.field("Plan", repo.rel(path))
    render.field("Sections", f"{len(present)}/{len(REQUIRED)} filled")
    if missing:
        render.heading("\nMissing sections")
        for s in missing:
            render.bullet(s, mark="✗")
    if empty:
        render.heading("\nEmpty / placeholder sections")
        for s in empty:
            render.bullet(s, mark="•")
    print()
    if not missing and not empty:
        render.ok("plan covers every required perspective")
        return 0
    render.fail(f"plan incomplete: {len(missing)} missing, {len(empty)} empty")
    return 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
