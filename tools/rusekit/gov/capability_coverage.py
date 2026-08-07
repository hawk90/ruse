#!/usr/bin/env python3
"""gov capability_coverage — every capability must declare its PRD relationship (the ruse-native axis).

`parity_coverage` closes the *external-editor* axis (parity → PRD). But ruse-native product surface has no
parity — a shipped pillar like command traces (F-022/CAP-TRACE) sits outside the parity catalog entirely, so
parity coverage can be 100% green while a real feature is missing from the spec. `spec/capabilities.yaml` is
the inventory of the product surface; the gap this checker closes is a CAP that neither links to a PRD feature
nor *declares* that it intentionally has none.

Rule: every `CAP-*` must carry a `prd` field — either `prd: [F-...]` (the features it realizes) or an explicit
`prd: []` (a deliberate "no direct requirement": infrastructure/service, or a future/third-party capability
with no current feature). The capabilities model links to PRD "where useful", so a link is not forced — but a
*decision* is: an absent `prd` field is undeclared drift and FAILS. Also FAILS a `prd` link naming a feature
that does not exist. Reports the linked-vs-declared-empty split so the ruse-native coverage is a visible
number. Auto-discovered into `ruse gov check`.
"""
from __future__ import annotations

import argparse
import re
import sys

import yaml

from rusekit import render, repo  # noqa: E402

CAPS = "spec/capabilities.yaml"
PRD = "spec/PRD.yaml"


def load_caps() -> dict:
    with open(repo.path(CAPS), encoding="utf-8") as fh:
        doc = yaml.safe_load(fh) or {}
    caps = doc.get("capabilities")
    if isinstance(caps, dict):
        return caps
    # fallback: top-level CAP-* keys
    return {k: v for k, v in doc.items() if isinstance(k, str) and k.startswith("CAP-")}


def prd_feature_ids() -> set[str]:
    with open(repo.path(PRD), encoding="utf-8") as fh:
        txt = fh.read()
    return set(re.findall(r"^  (F-\d+):", txt, re.M))


def evaluate(caps: dict, features: set[str]) -> dict:
    undeclared, broken, linked, declared_empty = [], [], 0, 0
    for cid, entry in caps.items():
        if not isinstance(entry, dict):
            continue
        if "prd" not in entry:
            undeclared.append(cid)
            continue
        refs = entry.get("prd") or []
        if refs:
            linked += 1
            for f in refs:
                if f not in features:
                    broken.append((cid, f))
        else:
            declared_empty += 1
    return {"total": len(caps), "undeclared": undeclared, "broken": broken,
            "linked": linked, "declared_empty": declared_empty}


def main(argv=None) -> int:
    argparse.ArgumentParser(prog="gov capability_coverage").parse_args(argv or [])
    caps = load_caps()
    r = evaluate(caps, prd_feature_ids())

    render.heading("gov capability_coverage (capability → PRD declaration)")
    render.field("Capabilities", str(r["total"]))
    render.field("Linked to a feature", str(r["linked"]))
    render.field("Declared prd: [] (infra/future)", str(r["declared_empty"]))
    render.field("Undeclared (no prd field)", str(len(r["undeclared"])))

    for cid in sorted(r["undeclared"]):
        render.bullet(f"{cid}: no `prd` field — declare `prd: [F-...]` or an explicit `prd: []` (infra/future)",
                      mark="!")
    for cid, f in r["broken"]:
        render.bullet(f"{cid}: prd link `{f}` is not a real PRD feature", mark="!")

    if r["undeclared"] or r["broken"]:
        render.fail(f"{len(r['undeclared'])} undeclared + {len(r['broken'])} broken capability→PRD link(s)")
        return 1
    render.ok(f"every capability declares its PRD relationship "
              f"({r['linked']} linked, {r['declared_empty']} explicit-empty)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
