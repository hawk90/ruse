#!/usr/bin/env python3
"""gov parity_coverage — every targeted parity item must derive into the PRD (enforces D-007).

The PRD (`spec/PRD.yaml`) is supposed to be *derived* from what upstreams actually do: every feature carries
`trace.parity` naming the parity IDs it satisfies. Nothing enforced that derivation, so ~3/4 of the catalog was
orphaned (defined in parity, referenced by no requirement) — the product spec silently drifted from the parity
source while design docs stayed in sync. This checker closes that loop (D-007's "re-evaluate as parity CI
matures").

TWO SOURCES OF TARGETED ITEMS, and the second one is why this file was rewritten:

  1. `docs/parity/*.md` — the HAND-AUTHORED catalog. Targeted = Target is `L1`/`L2` AND Compat is not
     `Unsupported` / `Intentionally-different` (the table's own columns encode intent, so L3 and the
     deliberate-divergence rows auto-exclude).
  2. `spec/parity/inventory/*/*.yaml` — the MACHINE CENSUS (D-043), which superseded the catalog as the
     source of truth. Targeted = `status: targeted`.

Reading only source 1 was a live defect. As the census burn-down proceeds, features repoint `trace.parity`
from legacy IDs onto census IDs and the legacy rows are deleted — so this gate's denominator was shrinking
toward zero while the thing it is supposed to guard was moving into a file it could not see. It would have
ended up PASSING while checking nothing. It was caught by hand: `nvim.mapmode.lang` sat `targeted` with no
owning feature and no gate said a word.

ROLE SCOPING. Only upstreams whose `role` in `spec/parity/upstreams.yaml` means "ruse intends compatibility"
(primary / baseline / secondary) carry a PRD-coverage obligation. `role: reference` (helix) is pinned as
EVIDENCE for a design decision, not as a compatibility target — its own inventory headers say
`not_a_parity_target: … these counts are never a coverage ratio`. Requiring PRD coverage for a reference
upstream would force ruse to promise features it deliberately does not promise. Excluded items are counted and
reported, never silently dropped.

  - *covered* = the ID appears in some feature's `trace.parity` (any `parity:` list) in spec/PRD.yaml.
  - *backlogged* = the ID is listed in `spec/parity-backlog.yaml` — the honest, tracked debt ledger for
    known-uncovered items (burning it down = the PRD/CFG backfill work).

FAILS (blocking gate) on any targeted parity ID that is neither covered nor backlogged — i.e. new drift, or
untracked debt. WARNS on backlog entries that are now covered (stale — clean up) or that don't name a real
targeted parity ID (typo/removed). Auto-discovered into `ruse gov check`.
"""
from __future__ import annotations

import argparse
import glob
import re
import sys

import yaml

from rusekit import render, repo  # noqa: E402

PARITY_GLOB = "docs/parity/*.md"
CENSUS_GLOB = "spec/parity/inventory/*/*.yaml"
UPSTREAMS = "spec/parity/upstreams.yaml"
PRD = "spec/PRD.yaml"
BACKLOG = "spec/parity-backlog.yaml"

# Upstream roles that mean "ruse intends compatibility" and therefore carry a PRD-coverage obligation.
# `reference` is deliberately absent — see the module docstring.
TARGET_ROLES = {"primary", "baseline", "secondary"}

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
    """Legacy parity IDs that MUST be covered: Target L1/L2 and not an excluded Compat."""
    out = set()
    for r in rows:
        if r["target"] in ("L1", "L2") and r["compat"] is None:
            out.add(r["id"])
    return out


def upstream_roles() -> dict:
    """editor -> role, from the pins. Absent file / absent role reads as no role, which excludes."""
    try:
        with open(repo.path(UPSTREAMS), encoding="utf-8") as fh:
            doc = yaml.safe_load(fh) or {}
    except FileNotFoundError:
        return {}
    return {k: (v or {}).get("role") for k, v in (doc.get("upstreams") or {}).items()}


def census_targeted() -> tuple[set[str], list[tuple[str, str, str]]]:
    """(ids obliged to be covered, [(id, editor, role) excluded because the upstream is not a target]).

    An inventory doc may carry its own `role`; the pin in upstreams.yaml wins if both are present, because the
    pin is where the decision is recorded and the inventory is generated FROM it.
    """
    roles = upstream_roles()
    obliged: set[str] = set()
    excluded: list[tuple[str, str, str]] = []
    for path in sorted(glob.glob(repo.path(CENSUS_GLOB))):
        with open(path, encoding="utf-8") as fh:
            doc = yaml.safe_load(fh) or {}
        editor = str(doc.get("upstream") or "")
        role = roles.get(editor, doc.get("role"))
        for item in doc.get("items") or []:
            if not isinstance(item, dict) or item.get("status") != "targeted":
                continue
            iid = str(item.get("id") or "")
            if not iid:
                continue
            if role in TARGET_ROLES:
                obliged.add(iid)
            else:
                excluded.append((iid, editor, str(role)))
    return obliged, excluded


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
    legacy = targeted_ids(rows)
    census, excluded_by_role = census_targeted()
    targeted = legacy | census
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
    render.field("  from the census (D-043)", str(len(census)))
    render.field("  from docs/parity/*.md (legacy)", str(len(legacy)))
    render.field("Excluded: upstream role is not a target", str(len(excluded_by_role)))
    render.field("Covered by PRD", str(len(covered)))
    render.field("Backlogged (tracked debt)", str(len(backlogged & orphaned)))
    render.field("Uncovered (untracked)", str(len(uncovered)))

    for pid in sorted(uncovered)[:40]:
        src = "census `status: targeted`" if pid in census else "legacy row, Target L1/L2"
        render.bullet(f"{pid}: {src} but in neither PRD trace.parity nor the backlog", mark="!")
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
              f"({len(covered)} covered — {len(census)} census + {len(legacy)} legacy, "
              f"{len(backlogged & orphaned)} backlogged)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
