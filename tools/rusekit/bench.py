#!/usr/bin/env python3
"""ruse bench — run the workspace benchmarks and (optionally) compare them to the committed baseline.

Makes "measure" one command instead of a hand-rolled `cargo bench` incantation, so a perf claim is always
backed by a fresh number (D-019). `gov perf` gates that every `[[bench]]` has a `spec/perf-baseline.yaml`
entry; this runner produces the numbers. With `--check` it flags a case that regressed past a generous
threshold vs the baseline — the shape a nightly CI job uses (criterion numbers are machine-relative, so the
threshold is deliberately loose and this is a WARN, never a hard PR gate).
"""
from __future__ import annotations

import argparse
import re
import subprocess

import yaml

from rusekit import render, repo

BASELINE = "spec/perf-baseline.yaml"
# criterion line: "group/id   time:   [lo_v lo_u  est_v est_u  hi_v hi_u]" — capture the central estimate.
LINE = re.compile(r"^(\S+)\s+time:\s+\[[\d.]+ \w+\s+([\d.]+) (\w+)\s+[\d.]+ \w+\]")
# unit → seconds (criterion emits ps/ns/us/µs/ms/s).
UNIT = {"ps": 1e-12, "ns": 1e-9, "us": 1e-6, "µs": 1e-6, "ms": 1e-3, "s": 1.0}
# A regression must be this many times slower than baseline to warn (machine-relative; loose on purpose).
THRESHOLD = 2.0


def to_seconds(value: float, unit: str) -> float:
    return value * UNIT.get(unit, float("nan"))


def parse_baseline() -> dict[str, float]:
    p = repo.path(BASELINE)
    with open(p, encoding="utf-8") as fh:
        data = yaml.safe_load(fh) or {}
    out: dict[str, float] = {}
    for cases in (data.get("benches") or {}).values():
        for case, txt in (cases or {}).items():
            m = re.match(r"([\d.]+)\s*(\w+)", str(txt))
            if m:
                out[case] = to_seconds(float(m.group(1)), m.group(2))
    return out


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(prog="ruse bench")
    ap.add_argument("--check", action="store_true",
                    help="compare the run to spec/perf-baseline.yaml and warn on regressions")
    ap.add_argument("--measurement-time", default="2",
                    help="criterion measurement seconds per case (default 2 — a quick relative run)")
    args = ap.parse_args(argv or [])

    render.heading("ruse bench (workspace benchmarks)")
    # `--benches` selects only the criterion `[[bench]]` targets — NOT the lib/bin unit tests, which the
    # bench profile would otherwise run under the std harness that rejects criterion's flags.
    cmd = ["cargo", "bench", "--workspace", "--benches", "--",
           "--warm-up-time", "0.3", "--measurement-time", args.measurement_time]
    proc = subprocess.run(cmd, cwd=repo.ROOT, capture_output=True, text=True)
    results: dict[str, tuple[float, str, str]] = {}
    for line in proc.stdout.splitlines():
        m = LINE.match(line)
        if m:
            case, val, unit = m.group(1), float(m.group(2)), m.group(3)
            results[case] = (to_seconds(val, unit), val, unit)

    if not results:
        render.fail("no benchmark results parsed — is the toolchain present? (see stderr)")
        if proc.returncode != 0:
            render.warn(proc.stderr.strip()[-800:])
        return 1

    baseline = parse_baseline() if args.check else {}
    regressions = []
    for case in sorted(results):
        secs, val, unit = results[case]
        if args.check and case in baseline and baseline[case] > 0:
            ratio = secs / baseline[case]
            flag = f"  ({ratio:.1f}× baseline)" if ratio >= THRESHOLD else ""
            if ratio >= THRESHOLD:
                regressions.append((case, ratio))
            render.field(case, f"{val:g} {unit}{flag}", width=40)
        else:
            render.field(case, f"{val:g} {unit}", width=40)

    if args.check and regressions:
        for case, ratio in regressions:
            render.bullet(f"{case} is {ratio:.1f}× the baseline (>{THRESHOLD:g}×) — re-measure and, if "
                          f"real, update {BASELINE} or fix it", mark="!")
        render.warn(f"{len(regressions)} case(s) regressed past {THRESHOLD:g}× (machine-relative — verify "
                    "before acting)")
        return 0  # WARN, not a hard fail (D-019: trend, not a PR gate)

    render.ok(f"{len(results)} benchmark case(s) ran"
              + (" — none past threshold" if args.check else ""))
    return 0


if __name__ == "__main__":
    import sys
    raise SystemExit(main(sys.argv[1:]))
