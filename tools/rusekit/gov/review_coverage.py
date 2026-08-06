#!/usr/bin/env python3
"""gov review_coverage — make the review rubric's machine-coverage first-class and drift-proof.

The review rubric (spec/review-axes.yaml, ~566 axes) tags each axis with a review METHOD
(machine | mixed | llm | manual). Some of those axes are ACTUALLY mechanized by a checker already —
e.g. ARCH circular-dependencies + dependency-direction by `dependency-check`, design↔code drift by
`design_code`. Until now that linkage lived only in a human's head, so the rubric read as "all manual/LLM"
even where a machine already enforces it, and nothing guarded the link if a checker was renamed.

This checker makes the linkage a first-class field: an axis may carry `automated_by: <checker>` naming the
verify step (spec/verification.yaml) or gov checker (tools/rusekit/gov/*.py) that mechanically enforces it.
Then it:
  - FAILS if any `automated_by` names a checker that does not exist (a broken/renamed link — drift);
  - WARNS for each `machine`-method axis with no `automated_by` (a claimed-automatable axis not yet wired);
  - reports per-domain machine-coverage counts, so "how much of the rubric is mechanized" is a number.

Portable engine; auto-discovered into `ruse gov check`.
"""
from __future__ import annotations

import argparse
import glob
import os
import sys

import yaml

from rusekit import repo, render  # noqa: E402

AXES = "spec/review-axes.yaml"
VERIFICATION = "spec/verification.yaml"


def valid_checkers() -> set[str]:
    """The namespace an `automated_by` may reference: verify-step names + gov checker module names."""
    names: set[str] = set()
    with open(repo.path(VERIFICATION), encoding="utf-8") as fh:
        for step in (yaml.safe_load(fh) or {}).get("steps", []):
            if step.get("name"):
                names.add(step["name"])
    for p in glob.glob(repo.path("tools", "rusekit", "gov", "*.py")):
        base = os.path.basename(p)
        if base != "__init__.py":
            names.add(base[:-3])  # module name, e.g. design_code
    return names


def load_axes(doc: dict) -> list[dict]:
    """Flatten domains -> axes with the effective method, domain id, and any automated_by."""
    axes = []
    for dom in doc.get("domains", []) or []:
        dom_id = dom.get("id", "?")
        default_method = dom.get("default_method", "llm")
        for a in dom.get("axes", []) or []:
            axes.append({
                "id": a.get("id"),
                "domain": dom_id,
                "method": a.get("method") or default_method,
                "automated_by": a.get("automated_by"),
            })
    return axes


def evaluate(axes: list[dict], checkers: set[str]) -> dict:
    """Broken links, machine-without-automation gaps, and per-domain coverage counts."""
    broken, gaps = [], []
    machine_wired = 0
    per_domain: dict[str, dict] = {}
    for a in axes:
        d = per_domain.setdefault(a["domain"], {"total": 0, "automated": 0, "machine": 0})
        d["total"] += 1
        if a["method"] == "machine":
            d["machine"] += 1
        if a["automated_by"]:
            d["automated"] += 1
            if a["method"] == "machine":
                machine_wired += 1
            if a["automated_by"] not in checkers:
                broken.append((a["id"], a["automated_by"]))
        elif a["method"] == "machine":
            gaps.append(a["id"])
    return {"broken": broken, "gaps": gaps, "per_domain": per_domain,
            "total": len(axes), "machine_wired": machine_wired,
            "automated": sum(d["automated"] for d in per_domain.values())}


def main(argv=None) -> int:
    argparse.ArgumentParser(prog="gov review_coverage").parse_args(argv or [])
    with open(repo.path(AXES), encoding="utf-8") as fh:
        doc = yaml.safe_load(fh) or {}
    axes = load_axes(doc)
    checkers = valid_checkers()
    r = evaluate(axes, checkers)

    render.heading("gov review_coverage (review-axes machine coverage)")
    render.field("Axes total", str(r["total"]))
    render.field("Axes with an automating checker", str(r["automated"]))
    render.field("method=machine, wired", str(r["machine_wired"]))
    render.field("method=machine, unwired (backlog)", str(len(r["gaps"])))
    render.field("Broken automated_by links", str(len(r["broken"])))
    # Per-domain, only where automation exists — a compact gauge, never a per-axis dump.
    for domain, d in sorted(r["per_domain"].items()):
        if d["automated"]:
            render.bullet(f"{domain}: {d['automated']}/{d['total']} axes wired to a checker")
    # Broken links are the only per-item findings (they are fatal drift).
    for axis, checker in r["broken"]:
        render.bullet(f"{axis}: automated_by `{checker}` is not a known verify step or gov checker "
                      "— fix the link or the checker name", mark="!")

    if r["broken"]:
        render.fail(f"{len(r['broken'])} broken automated_by link(s) — the rubric points at a missing checker")
        return 1
    if r["gaps"]:
        # A backlog metric, not a per-run nag: the rubric optimistically tagged many axes `machine` before a
        # checker existed. Surface the count so coverage is honest; wiring them is ongoing work, not a failure.
        render.warn(f"{len(r['gaps'])} axes claim method=machine but are not wired to a checker yet "
                    "(coverage backlog — informational)")
        return 0
    render.ok(f"all {r['automated']} automated_by links resolve; every method=machine axis is wired")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
