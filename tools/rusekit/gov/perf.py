#!/usr/bin/env python3
"""gov perf — every `[[bench]]` must have a committed baseline (measure before claiming, D-019).

The failure mode this gate exists to prevent: a performance CLAIM asserted by analogy instead of measured
(a real defect caught in review — a caching change was shipped as "the same fast class" without a number).
The systemic fix is not another manual benchmark; it is to make measuring part of the harness. `ruse bench`
runs the benches and (re)writes `spec/perf-baseline.yaml`; THIS checker gates, deterministically and
without running anything, that every registered `[[bench]]` appears in that baseline and vice-versa. So a
new perf-sensitive bench cannot land without its numbers on record, and a deleted bench cannot leave a
stale claim behind.

Deliberately NOT a regression gate: criterion numbers are machine-relative (D-019 says budgets gate on a
FIXED machine, nightly, trend+warn — a live comparison belongs in a scheduled CI job, not a PR gate that a
noisy shared runner would flake). This gate only checks COVERAGE + SYNC, which is deterministic.

Auto-discovered into `ruse gov check`.
"""
from __future__ import annotations

import argparse
import glob
import os
import re
import sys

import yaml

from rusekit import render, repo  # noqa: E402

BASELINE = "spec/perf-baseline.yaml"
MANIFESTS = ["crates/core/Cargo.toml", "apps/tui/Cargo.toml"]


def registered_benches() -> set[str]:
    """The `name` of every `[[bench]]` across the workspace manifests."""
    names: set[str] = set()
    for manifest in MANIFESTS:
        p = repo.path(manifest)
        if not os.path.exists(p):
            continue
        with open(p, encoding="utf-8") as fh:
            in_bench = False
            for line in fh:
                s = line.strip()
                if s == "[[bench]]":
                    in_bench = True
                    continue
                if s.startswith("["):  # any other table ends the bench block
                    in_bench = False
                if in_bench:
                    m = re.match(r'name\s*=\s*"([^"]+)"', s)
                    if m:
                        names.add(m.group(1))
                        in_bench = False
    return names


def baseline_benches() -> set[str]:
    p = repo.path(BASELINE)
    if not os.path.exists(p):
        return set()
    with open(p, encoding="utf-8") as fh:
        data = yaml.safe_load(fh) or {}
    return set((data.get("benches") or {}).keys())


def main(argv=None) -> int:
    argparse.ArgumentParser(prog="gov perf").parse_args(argv or [])
    render.heading("gov perf (every [[bench]] has a committed baseline)")

    registered = registered_benches()
    recorded = baseline_benches()

    render.field("Registered benches", str(len(registered)))
    render.field("Baseline entries", str(len(recorded)))

    if not os.path.exists(repo.path(BASELINE)):
        render.fail(f"no baseline at {BASELINE} — run `ruse bench` to record it")
        return 1

    missing = sorted(registered - recorded)   # a bench with no recorded numbers
    stale = sorted(recorded - registered)      # a baseline entry for a removed bench

    for b in missing:
        render.bullet(f"[[bench]] {b} has no baseline entry — run `ruse bench` and commit "
                      f"{BASELINE} (a perf claim needs a number)", mark="!")
    for b in stale:
        render.bullet(f"baseline lists {b}, which is no longer a registered [[bench]] — remove the "
                      f"stale entry from {BASELINE}", mark="!")

    if missing or stale:
        render.fail(f"{len(missing)} bench(es) unrecorded, {len(stale)} stale baseline entr(ies)")
        return 1

    render.ok(f"all {len(registered)} benches have a committed baseline (regenerate with `ruse bench`)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
