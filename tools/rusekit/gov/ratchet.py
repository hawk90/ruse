#!/usr/bin/env python3
"""gov ratchet — tracked-debt buckets may never exceed their committed ceiling (turns "green" into progress).

Four debt ledgers are designed to keep the build green while their number "trends to zero": config keys with
`derives_from: []`, capabilities with an empty `prd:`, hand-authored parity IDs still cited by the PRD, and
the parity backlog. Nothing enforced the trend, so "all green" was a stable equilibrium at high debt, not a
milestone (architecture-review finding). This gate stores a ceiling per bucket in spec/parity/ratchet.yaml; a
bucket that RISES above its ceiling FAILS. Lower a ceiling when you burn the debt down and the gate holds the
new floor; raising one is a deliberate, reviewable edit — which is the whole point.

`unclassified` census items are deliberately NOT ratcheted: they grow when a new upstream is censused
(discovery), which the census WANTS; partial classification is already blocked per-surface by
parity_discovery. Auto-discovered into `ruse gov check`.
"""
from __future__ import annotations

import argparse
import sys

import yaml

from rusekit import render, repo  # noqa: E402
from rusekit.gov import parity_discovery  # noqa: E402

BASELINE = "spec/parity/ratchet.yaml"


def _load(path: str) -> dict:
    try:
        with open(repo.path(path), encoding="utf-8") as fh:
            return yaml.safe_load(fh) or {}
    except FileNotFoundError:
        return {}


def current_buckets() -> dict:
    settings = (_load("spec/config-schema.yaml").get("settings")) or {}
    config_untraced = sum(1 for v in settings.values()
                          if isinstance(v, dict) and v.get("derives_from") == [])
    caps = (_load("spec/capabilities.yaml").get("capabilities")) or {}
    cap_untraced = sum(1 for v in caps.values() if isinstance(v, dict) and not v.get("prd"))
    bl = _load("spec/parity-backlog.yaml")
    backlog = len(bl.get("items") or bl.get("backlog") or []) if isinstance(bl, dict) else 0
    legacy = len(parity_discovery.check().get("legacy_in_prd") or [])
    return {"config_untraced": config_untraced, "cap_untraced": cap_untraced,
            "backlog": backlog, "legacy_in_prd": legacy}


def main(argv=None) -> int:
    argparse.ArgumentParser(prog="gov ratchet").parse_args(argv or [])
    baseline = (_load(BASELINE).get("buckets")) or {}
    current = current_buckets()

    render.heading("gov ratchet (tracked-debt ceilings)")
    if not baseline:
        render.warn(f"no baseline at {BASELINE} — create it with the current counts to arm the ratchet")
        for b, v in sorted(current.items()):
            render.field(b, str(v))
        return 0

    rose, tighten = [], []
    for b, cur in sorted(current.items()):
        ceil = baseline.get(b)
        render.field(b, f"{cur} / ceiling {ceil if ceil is not None else '—'}")
        if ceil is None:
            tighten.append(f"{b}: {cur} has no declared ceiling — add one to ratchet.yaml")
        elif cur > ceil:
            rose.append((b, ceil, cur))
        elif cur < ceil:
            tighten.append(f"{b}: {cur} is below ceiling {ceil} — lower the ceiling to lock the gain")

    for b, ceil, cur in rose:
        render.bullet(f"{b} rose to {cur}, above ceiling {ceil} — trace/burn it down, or justify raising "
                      f"the ceiling in ratchet.yaml", mark="!")
    if rose:
        render.fail(f"{len(rose)} tracked-debt bucket(s) rose above ceiling")
        return 1
    for t in tighten:
        render.warn(t)
    render.ok("no tracked-debt bucket exceeds its ceiling")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
