#!/usr/bin/env python3
"""gov design_backing — depth-honesty for the parity→PRD→design tower.

The parity-coverage gate (parity_coverage) proves *breadth*: every targeted parity item has an owning PRD
feature. But breadth linkage can lie about *depth* — a feature can claim `trace.design` while pointing at the
parity checklist itself (circular: the catalog is WHAT-to-match, not HOW), or at a file that does not exist.
The real risk isn't shallow parity; it's shallow parity that *looks* deep. This checker keeps depth honest:

  - FAILS on a **circular** `trace.design` — any entry under `docs/parity/` (a design trace must point at an
    architecture/design doc, never back at the parity catalog it derives from).
  - FAILS on a **broken** `trace.design` link — an entry whose file does not exist.
  - Reports, as a visible metric (not a block — depth is authored per-feature at build time, RFC-0012), how
    many features carry `trace.parity` but have **no real design backing** (breadth-only). This is the
    honest readiness number; it should trend down as features are designed, and it must never be hidden by a
    circular self-reference.

Auto-discovered into `ruse gov check`. PRD `trace.design` paths are relative to `spec/` (where PRD.yaml lives).
"""
from __future__ import annotations

import argparse
import os
import sys

import yaml

from rusekit import render, repo  # noqa: E402

PRD = "spec/PRD.yaml"


def resolve(entry: str) -> str:
    """A trace.design path (relative to spec/) with any #anchor stripped → absolute repo path."""
    path = entry.split("#", 1)[0]
    return os.path.normpath(repo.path("spec", path))


def evaluate(features: dict) -> dict:
    circular, broken, breadth_only = [], [], []
    for fid, feat in features.items():
        trace = feat.get("trace") or {}
        parity = trace.get("parity") or []
        design = trace.get("design") or []
        real = []
        for entry in design:
            if "docs/parity/" in entry:
                circular.append((fid, entry))
                continue
            if not os.path.isfile(resolve(entry)):
                broken.append((fid, entry))
                continue
            real.append(entry)
        if parity and not real:
            breadth_only.append(fid)
    return {"circular": circular, "broken": broken, "breadth_only": breadth_only,
            "total": len(features)}


def main(argv=None) -> int:
    argparse.ArgumentParser(prog="gov design_backing").parse_args(argv or [])
    with open(repo.path(PRD), encoding="utf-8") as fh:
        doc = yaml.safe_load(fh) or {}
    features = doc.get("features") or {}
    r = evaluate(features)

    render.heading("gov design_backing (PRD trace.design honesty)")
    render.field("Features", str(r["total"]))
    render.field("Circular design refs", str(len(r["circular"])))
    render.field("Broken design links", str(len(r["broken"])))
    render.field("Breadth-only (no design yet)", str(len(r["breadth_only"])))

    for fid, entry in r["circular"]:
        render.bullet(f"{fid}: trace.design points at the parity catalog `{entry}` — circular; point at an "
                      "architecture/design doc instead", mark="!")
    for fid, entry in r["broken"]:
        render.bullet(f"{fid}: trace.design `{entry}` does not exist", mark="!")

    if r["circular"] or r["broken"]:
        render.fail(f"{len(r['circular'])} circular + {len(r['broken'])} broken trace.design ref(s)")
        return 1
    if r["breadth_only"]:
        # Not a failure: depth is authored per-feature at build (RFC-0012). Just keep the number visible.
        render.warn(f"{len(r['breadth_only'])} feature(s) carry parity but no design backing yet "
                    f"(breadth-only, informational): {', '.join(sorted(r['breadth_only']))}")
        return 0
    render.ok("every feature with parity has real (non-circular, resolvable) design backing")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
