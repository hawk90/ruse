#!/usr/bin/env python3
"""gov parity_coverage — the parity catalog is the upstream source; PRD must derive from it (enforces D-007).

The `docs/parity/*.md` catalog is the well-built source of truth for "what Vim/Emacs/… do". The PRD
(`spec/PRD.yaml`) is supposed to be *derived* from it: every feature carries `trace.parity` naming the parity
IDs it satisfies. Until now nothing enforced that derivation, so ~3/4 of the catalog was orphaned (defined in
parity, referenced by no requirement) — the product spec silently drifted from the parity source while design
docs stayed in sync. This checker closes that loop (D-007's "re-evaluate as parity CI matures").

Rule — a **targeted** parity item must be covered by the PRD or explicitly backlogged:
  - *targeted* = the parity row's Target is `L1`/`L2` AND its Compat is NOT `Unsupported` /
    `Intentionally-different` (the parity doc's own columns encode intent: L3 and Unsupported/
    Intentionally-different are deliberately not coverage targets, so they auto-exclude).
  - *covered* = the ID appears in some feature's `trace.parity` (any `parity:` list) in spec/PRD.yaml.
  - *backlogged* = the ID is listed in `spec/parity-backlog.yaml` — the honest, tracked debt ledger for
    known-uncovered items (burning it down = the PRD/CFG backfill work).

FAILS (blocking gate) on any targeted parity ID that is neither covered nor backlogged — i.e. new drift, or
untracked debt. WARNS on backlog entries that are now covered (stale — clean up) or that don't name a real
targeted parity ID (typo/removed). Reports the backlog size so the uncovered-debt number is visible and
should trend to zero. Auto-discovered into `ruse gov check`.
"""
from __future__ import annotations

import argparse
import glob
import re
import sys

import yaml

from rusekit import render, repo  # noqa: E402

PARITY_GLOB = "docs/parity/*.md"
PRD = "spec/PRD.yaml"
BACKLOG = "spec/parity-backlog.yaml"

# The known parity-ID prefixes (docs/parity/README.md). An ID is `<PREFIX>-<UPPER/DIGIT/->`.
ID_RE = re.compile(r"^(?:VIM|NVIM|EMACS|COM|TERM|WS|REM|ECO|NAT)-[A-Z0-9][A-Z0-9-]*$")
TARGET_RE = re.compile(r"^L[123]$")
# Compat values that mean "deliberately not a parity target" → auto-excluded from the coverage requirement.
EXCLUDED_COMPAT = {"unsupported", "intentionally-different", "intentionally different"}


def parity_rows() -> list[dict]:
    """Every parity table row as {id, target, compat, doc}. Robust to column order: within a row we locate the
    Target cell (L1/L2/L3) and the Compat cell by value, not by position."""
    rows = []
    for path in sorted(glob.glob(repo.path(PARITY_GLOB))):
        doc = path.rsplit("/", 1)[-1]
        with open(path, encoding="utf-8") as fh:
            for line in fh:
                line = line.strip()
                if not line.startswith("|") or "---" in line:
                    continue
                cells = [c.strip() for c in line.strip("|").split("|")]
                if not cells:
                    continue
                pid = cells[0]
                if not ID_RE.match(pid):
                    continue
                target = next((c for c in cells if TARGET_RE.match(c)), None)
                compat = next((c for c in cells if c.lower() in EXCLUDED_COMPAT), None)
                rows.append({"id": pid, "target": target, "compat": compat, "doc": doc})
    return rows


def targeted_ids(rows: list[dict]) -> set[str]:
    """Parity IDs that MUST be covered: Target L1/L2 and not an excluded Compat."""
    out = set()
    for r in rows:
        if r["target"] in ("L1", "L2") and r["compat"] is None:
            out.add(r["id"])
    return out


def _collect_parity(node, acc: set[str]) -> None:
    """Walk the PRD YAML collecting every value under any key named `parity` (handles trace.parity)."""
    if isinstance(node, dict):
        for k, v in node.items():
            if k == "parity" and isinstance(v, list):
                acc.update(str(x) for x in v)
            else:
                _collect_parity(v, acc)
    elif isinstance(node, list):
        for v in node:
            _collect_parity(v, acc)


def prd_referenced() -> set[str]:
    with open(repo.path(PRD), encoding="utf-8") as fh:
        doc = yaml.safe_load(fh) or {}
    acc: set[str] = set()
    _collect_parity(doc, acc)
    return acc


def backlog_ids() -> set[str]:
    p = repo.path(BACKLOG)
    try:
        with open(p, encoding="utf-8") as fh:
            doc = yaml.safe_load(fh) or {}
    except FileNotFoundError:
        return set()
    return {str(e["id"]) for e in (doc.get("backlog") or []) if isinstance(e, dict) and e.get("id")}


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(prog="gov parity_coverage")
    ap.add_argument("--list-uncovered", action="store_true",
                    help="print the uncovered targeted parity IDs (to seed the backlog) and exit")
    args = ap.parse_args(argv or [])

    rows = parity_rows()
    targeted = targeted_ids(rows)
    referenced = prd_referenced()
    backlogged = backlog_ids()

    covered = targeted & referenced
    orphaned = targeted - referenced
    uncovered = orphaned - backlogged          # neither in PRD nor backlog → blocking
    stale = backlogged & referenced            # backlogged but now covered → clean up (warn)
    unknown = backlogged - targeted            # backlog names a non-targeted / typo ID (warn)

    if args.list_uncovered:
        for pid in sorted(uncovered):
            print(pid)
        return 0

    render.heading("gov parity_coverage (parity → PRD derivation)")
    render.field("Targeted parity items", str(len(targeted)))
    render.field("Covered by PRD", str(len(covered)))
    render.field("Backlogged (tracked debt)", str(len(backlogged & orphaned)))
    render.field("Uncovered (untracked)", str(len(uncovered)))

    for pid in sorted(uncovered)[:40]:
        render.bullet(f"{pid}: targeted (L1/L2) but in neither PRD trace.parity nor the backlog", mark="!")
    if len(uncovered) > 40:
        render.bullet(f"… and {len(uncovered) - 40} more", mark="!")
    for pid in sorted(stale):
        render.bullet(f"{pid}: in the backlog but now covered by the PRD — remove the backlog entry", mark="!")
    for pid in sorted(unknown):
        render.bullet(f"{pid}: backlog names an unknown / non-targeted parity ID — fix or remove", mark="!")

    if uncovered:
        render.fail(f"{len(uncovered)} targeted parity item(s) not derived into the PRD and not backlogged")
        return 1
    if stale or unknown:
        render.warn(f"{len(stale)} stale + {len(unknown)} unknown backlog entr(y/ies) — housekeeping")
        return 0
    render.ok(f"every targeted parity item is covered by the PRD or tracked in the backlog "
              f"({len(covered)} covered, {len(backlogged & orphaned)} backlogged)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
