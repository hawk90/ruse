#!/usr/bin/env python3
"""gov prd_cfg — every config setting must declare which PRD feature it derives from (the PRD -> CFG axis).

`parity_coverage` closes parity -> PRD and `capability_coverage` closes CAP -> PRD. The third edge of the
traceability chain — the CONFIG surface — had no such gate: a setting key could appear in
`spec/config-schema.yaml` with no statement of which requirement it realizes, so the config layer could drift
from the PRD exactly the way the PRD once drifted from the parity catalog. This checker closes that edge.

Rule: every setting key MUST carry a `derives_from` field — either `derives_from: [F-...]` (the feature(s) the
setting realizes) or an explicit `derives_from: []` (a deliberate "no owning feature yet": infrastructure keys,
or a knob whose feature is not yet written). An ABSENT field is undeclared drift and FAILS. A `derives_from`
naming a feature that does not exist in `spec/PRD.yaml` also FAILS (a dangling trace is worse than none). An
explicit `[]` is legal, counted, and surfaced as a WARN ("N config keys not yet traced to a feature") — it is
NEVER a hard fail, mirroring how parity_coverage/capability_coverage keep the build green on tracked gaps while
keeping the number visible so it can trend to zero. Auto-discovered into `ruse gov check`.
"""
from __future__ import annotations

import argparse
import re
import sys

import yaml

from rusekit import render, repo  # noqa: E402

CFG = "spec/config-schema.yaml"
PRD = "spec/PRD.yaml"


def load_settings() -> dict:
    with open(repo.path(CFG), encoding="utf-8") as fh:
        doc = yaml.safe_load(fh) or {}
    settings = doc.get("settings")
    return settings if isinstance(settings, dict) else {}


def prd_feature_ids() -> set[str]:
    with open(repo.path(PRD), encoding="utf-8") as fh:
        txt = fh.read()
    return set(re.findall(r"^  (F-\d+):", txt, re.M))


def evaluate(settings: dict, features: set[str]) -> dict:
    undeclared, broken, linked, declared_empty = [], [], 0, 0
    for sid, entry in settings.items():
        if not isinstance(entry, dict):
            continue
        if "derives_from" not in entry:
            undeclared.append(sid)
            continue
        refs = entry.get("derives_from") or []
        if refs:
            linked += 1
            for f in refs:
                if f not in features:
                    broken.append((sid, f))
        else:
            declared_empty += 1
    return {"total": len(settings), "undeclared": undeclared, "broken": broken,
            "linked": linked, "declared_empty": declared_empty}


def main(argv=None) -> int:
    argparse.ArgumentParser(prog="gov prd_cfg").parse_args(argv or [])
    settings = load_settings()
    r = evaluate(settings, prd_feature_ids())

    render.heading("gov prd_cfg (config setting -> PRD derivation)")
    render.field("Settings", str(r["total"]))
    render.field("Linked to a feature", str(r["linked"]))
    render.field("Declared derives_from: [] (untraced)", str(r["declared_empty"]))
    render.field("Undeclared (no derives_from)", str(len(r["undeclared"])))

    for sid in sorted(r["undeclared"]):
        render.bullet(f"{sid}: no `derives_from` field — declare `derives_from: [F-...]` or an explicit "
                      f"`derives_from: []` (no owning feature yet)", mark="!")
    for sid, f in r["broken"]:
        render.bullet(f"{sid}: derives_from link `{f}` is not a real PRD feature", mark="!")

    if r["undeclared"] or r["broken"]:
        render.fail(f"{len(r['undeclared'])} undeclared + {len(r['broken'])} broken config->PRD link(s)")
        return 1
    if r["declared_empty"]:
        render.warn(f"{r['declared_empty']} config key(s) not yet traced to a feature "
                    f"(derives_from: []) — tracked gap, burn down as their features are written")
    render.ok(f"every config setting declares its PRD derivation "
              f"({r['linked']} linked, {r['declared_empty']} explicit-empty)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
