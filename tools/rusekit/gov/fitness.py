#!/usr/bin/env python3
"""gov fitness — validate the Architecture Fitness Functions (spec/fitness.yaml) and report coverage.

Fitness functions guard the repo's evolution over time (thresholds/trends), unlike feature tests. This
checker guards the *declaration's* integrity (ids FIT-*, known operator, numeric threshold, a `live`
function names an enforcing checker) and reports live vs planned. It does not itself MEASURE — a `live`
function is enforced by its named checker (e.g. dependency-check); `planned` ones await code + a metrics
source and must never silently pass. Portable engine; auto-discovered into `ruse gov check`.
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

FITNESS = "spec/fitness.yaml"
OPERATORS = {"equals", "less_than", "less_than_pct", "no_new"}
KINDS = {"atomic", "holistic", "continuous"}
STATUS = {"live", "planned"}
ID_RE = re.compile(r"^FIT-[A-Z0-9-]+$")


def load() -> list[dict]:
    p = repo.path(FITNESS)
    if yaml is None or not os.path.isfile(p):
        return []
    data = yaml.safe_load(open(p, encoding="utf-8")) or {}
    return data.get("functions") or []


def validate(functions: list[dict]):
    """Return (errors, warnings, coverage) with coverage = (live, planned) counts."""
    errors: list[str] = []
    warns: list[str] = []
    seen: set[str] = set()
    live = planned = 0
    for f in functions:
        fid = f.get("id", "<no-id>")
        if not ID_RE.match(fid or ""):
            errors.append(f"{fid}: id must match FIT-<UPPER>")
        if fid in seen:
            errors.append(f"duplicate function id {fid}")
        seen.add(fid)
        if not (f.get("metric") or "").strip():
            errors.append(f"{fid}: empty metric")
        if f.get("operator") not in OPERATORS:
            errors.append(f"{fid}: operator must be one of {sorted(OPERATORS)} (got {f.get('operator')!r})")
        if not isinstance(f.get("threshold"), (int, float)):
            errors.append(f"{fid}: threshold must be numeric (got {f.get('threshold')!r})")
        if f.get("kind") not in KINDS:
            errors.append(f"{fid}: kind must be one of {sorted(KINDS)} (got {f.get('kind')!r})")
        st = f.get("status")
        if st not in STATUS:
            errors.append(f"{fid}: status must be one of {sorted(STATUS)} (got {st!r})")
        elif st == "live":
            live += 1
            if not (f.get("check") or "").strip():
                errors.append(f"{fid}: status=live but no enforcing `check:` named")
        elif st == "planned":
            planned += 1
    return errors, warns, (live, planned)


def main(argv: list[str]) -> int:
    argparse.ArgumentParser(prog="ruse gov fitness").parse_args(argv)
    functions = load()
    errors, warns, (live, planned) = validate(functions)

    render.heading("gov fitness")
    render.field("Functions", str(len(functions)))
    render.field("Live", str(live))
    render.field("Planned", str(planned))
    for f in functions:
        mark = "✓" if f.get("status") == "live" else "·"
        op = f"{f.get('metric')} {f.get('operator')} {f.get('threshold')}"
        tail = f" → {f.get('check')}" if f.get("status") == "live" else " (planned)"
        render.bullet(f"{mark} {f.get('id')}: {op}{tail}")
    for w in warns:
        render.warn(w)
    for e in errors:
        render.fail(e)
    print()
    if errors:
        render.fail(f"gov fitness: FAIL ({len(errors)})")
        return 1
    render.ok(f"gov fitness: PASS ({live} live, {planned} planned)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
