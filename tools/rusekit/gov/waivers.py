#!/usr/bin/env python3
"""gov waivers — the governance waiver workflow (every suppressed rule is explicit, owned, expiring).

A waiver missing any required field is invalid; a waiver past its `expires` FAILS. This is what stops a
team from silently disabling a checker or growing an unbounded ignore list. `ruse pr check` calls
`active_waivers(rule)` so a required-but-waived gate is an owned, dated exception rather than a silent skip.

Portable: this checker + the spec/waivers.yaml schema are the reusable Repository-Governance-Plane engine.
"""
from __future__ import annotations

import argparse
import datetime
import os
import sys

from rusekit import repo, render  # noqa: E402

try:
    import yaml
except ImportError:  # pragma: no cover
    yaml = None

WAIVERS = "spec/waivers.yaml"
REQUIRED = ["id", "rule", "reason", "owner", "approved_by", "expires", "removal_spec"]


def load() -> list[dict]:
    p = repo.path(WAIVERS)
    if yaml is None or not os.path.isfile(p):
        return []
    data = yaml.safe_load(open(p, encoding="utf-8")) or {}
    return data.get("waivers") or []


def validate(waivers: list[dict], today: datetime.date | None = None):
    """Return (errors, warnings, active_waivers)."""
    today = today or datetime.date.today()
    errors: list[str] = []
    warns: list[str] = []
    active: list[dict] = []
    seen: set[str] = set()
    for w in waivers:
        wid = w.get("id", "<no-id>")
        missing = [f for f in REQUIRED if not w.get(f)]
        if missing:
            errors.append(f"{wid}: missing required field(s) {missing}")
            continue
        if wid in seen:
            errors.append(f"duplicate waiver id {wid}")
        seen.add(wid)
        exp = w["expires"]
        try:
            d = exp if isinstance(exp, datetime.date) else datetime.date.fromisoformat(str(exp))
        except ValueError:
            errors.append(f"{wid}: expires '{exp}' is not an ISO date (YYYY-MM-DD)")
            continue
        if d < today:
            errors.append(f"{wid}: EXPIRED {d} (waived '{w['rule']}') — remove it or renew via "
                          f"{w.get('removal_spec')}")
        else:
            active.append(w)
            days = (d - today).days
            if days <= 14:
                warns.append(f"{wid}: expires in {days}d ({d}) — plan removal ({w.get('removal_spec')})")
    return errors, warns, active


def active_waivers(rule: str | None = None) -> list[dict]:
    """Active (non-expired, well-formed) waivers, optionally filtered to one rule. For `pr check`."""
    _e, _w, active = validate(load())
    return [w for w in active if rule is None or w.get("rule") == rule]


def main(argv: list[str]) -> int:
    argparse.ArgumentParser(prog="ruse gov waivers").parse_args(argv)
    waivers = load()
    errors, warns, active = validate(waivers)

    render.heading("gov waivers")
    render.field("Declared", str(len(waivers)))
    render.field("Active", str(len(active)))
    for w in active:
        render.bullet(f"{w['id']}: waives '{w['rule']}' until {w['expires']} "
                      f"({w['owner']}, removal: {w['removal_spec']})")
    for wn in warns:
        render.warn(wn)
    for e in errors:
        render.fail(e)
    print()
    if errors:
        render.fail(f"gov waivers: FAIL ({len(errors)} — an expired/malformed waiver is a governance leak)")
        return 1
    render.ok(f"gov waivers: PASS ({len(active)} active, none expired)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
