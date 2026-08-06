#!/usr/bin/env python3
"""gov constitution — validate the Repository Constitution (spec/constitution.yaml) and report coverage.

The constitution is the standing invariant principles; this checker guards the *document's* integrity (every
article has an id/statement/enforcement; a `check`-enforced article names a checker; ids are unique, CON-*)
and reports how many articles are machine-enforced vs. review-only. It does NOT re-run the referenced checks
— those are their own gates. Portable: schema + this checker are the reusable Repository-Governance-Plane
engine; the articles are per-repo. Auto-discovered into `ruse gov check`.
"""
from __future__ import annotations

import argparse
import os
import re
import sys

from rusekit import repo, render  # noqa: E402

try:
    import yaml
except ImportError:  # pragma: no cover
    yaml = None

CONSTITUTION = "spec/constitution.yaml"
ENFORCEMENT = {"check", "review"}
ID_RE = re.compile(r"^CON-[A-Z0-9-]+$")


def load() -> list[dict]:
    p = repo.path(CONSTITUTION)
    if yaml is None or not os.path.isfile(p):
        return []
    data = yaml.safe_load(open(p, encoding="utf-8")) or {}
    return data.get("articles") or []


def validate(articles: list[dict]):
    """Return (errors, warnings, coverage) where coverage = (checked, review) counts."""
    errors: list[str] = []
    warns: list[str] = []
    seen: set[str] = set()
    checked = review = 0
    for a in articles:
        aid = a.get("id", "<no-id>")
        if not ID_RE.match(aid or ""):
            errors.append(f"{aid}: id must match CON-<UPPER>")
        if aid in seen:
            errors.append(f"duplicate article id {aid}")
        seen.add(aid)
        if not (a.get("statement") or "").strip():
            errors.append(f"{aid}: empty statement")
        enf = a.get("enforcement")
        if enf not in ENFORCEMENT:
            errors.append(f"{aid}: enforcement must be one of {sorted(ENFORCEMENT)} (got {enf!r})")
        elif enf == "check":
            checked += 1
            if not (a.get("check") or "").strip():
                errors.append(f"{aid}: enforcement=check but no `check:` checker named")
        elif enf == "review":
            review += 1
    return errors, warns, (checked, review)


def main(argv: list[str]) -> int:
    argparse.ArgumentParser(prog="ruse gov constitution").parse_args(argv)
    articles = load()
    errors, warns, (checked, review) = validate(articles)

    render.heading("gov constitution")
    render.field("Articles", str(len(articles)))
    render.field("Machine-enforced", str(checked))
    render.field("Review-enforced", str(review))
    for a in articles:
        mark = "✓" if a.get("enforcement") == "check" else "·"
        via = f" → {a.get('check')}" if a.get("enforcement") == "check" else " (review)"
        render.bullet(f"{mark} {a.get('id')}{via}")
    for w in warns:
        render.warn(w)
    for e in errors:
        render.fail(e)
    print()
    if errors:
        render.fail(f"gov constitution: FAIL ({len(errors)})")
        return 1
    render.ok(f"gov constitution: PASS ({checked} machine-enforced, {review} review)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
