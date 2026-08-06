#!/usr/bin/env python3
"""gov contracts — validate the Contract-Driven Development contracts (spec/contracts/*.yaml).

Contracts (API / protocol / file-format / …) are declared BEFORE their implementation (contract-first,
D-021 / INV-CONTRACT-FIRST). This checker guards each contract's integrity — id (CONTRACT-*), known kind /
compatibility / status, a non-empty guarantees list — and reports declared vs active. An `active` contract
(schema + conformance tests landed) must have existing `verified_by` paths; a `declared` one is a
pre-implementation promise. Portable engine; auto-discovered into `ruse gov check`.
"""
from __future__ import annotations

import argparse
import glob
import os
import re
import sys

from rusekit import repo, render  # noqa: E402

try:
    import yaml
except ImportError:  # pragma: no cover
    yaml = None

CONTRACT_DIR = "spec/contracts"
KINDS = {"api", "abi", "protocol", "file-format", "config", "error", "event", "cli"}
COMPAT = {"backward", "forward", "none"}
STATUS = {"declared", "active"}
ID_RE = re.compile(r"^CONTRACT-[A-Z0-9-]+$")


def load() -> list[tuple[str, dict]]:
    d = repo.path(CONTRACT_DIR)
    if yaml is None or not os.path.isdir(d):
        return []
    out = []
    for p in sorted(glob.glob(os.path.join(d, "*.yaml"))):
        out.append((repo.rel(p), yaml.safe_load(open(p, encoding="utf-8")) or {}))
    return out


def validate(contracts: list[tuple[str, dict]]):
    """Return (errors, warnings, coverage) with coverage = (declared, active)."""
    errors: list[str] = []
    warns: list[str] = []
    seen: set[str] = set()
    declared = active = 0
    for relf, c in contracts:
        cid = c.get("id", "<no-id>")
        where = f"{cid} ({relf})"
        if not ID_RE.match(cid or ""):
            errors.append(f"{where}: id must match CONTRACT-<UPPER>")
        if cid in seen:
            errors.append(f"duplicate contract id {cid}")
        seen.add(cid)
        if not (c.get("title") or "").strip():
            errors.append(f"{where}: empty title")
        if c.get("kind") not in KINDS:
            errors.append(f"{where}: kind must be one of {sorted(KINDS)} (got {c.get('kind')!r})")
        if c.get("compatibility") not in COMPAT:
            errors.append(f"{where}: compatibility must be one of {sorted(COMPAT)}")
        if not (c.get("guarantees") or []):
            errors.append(f"{where}: at least one guarantee is required")
        st = c.get("status")
        if st not in STATUS:
            errors.append(f"{where}: status must be one of {sorted(STATUS)} (got {st!r})")
        elif st == "active":
            active += 1
            for v in c.get("verified_by") or []:
                if not os.path.exists(repo.path(v)):
                    errors.append(f"{where}: active but verified_by '{v}' does not exist")
        elif st == "declared":
            declared += 1
    return errors, warns, (declared, active)


def main(argv: list[str]) -> int:
    argparse.ArgumentParser(prog="ruse gov contracts").parse_args(argv)
    contracts = load()
    errors, warns, (declared, active) = validate(contracts)

    render.heading("gov contracts")
    render.field("Contracts", str(len(contracts)))
    render.field("Active", str(active))
    render.field("Declared (pre-impl)", str(declared))
    for _relf, c in contracts:
        mark = "✓" if c.get("status") == "active" else "·"
        render.bullet(f"{mark} {c.get('id')} [{c.get('kind')}, {c.get('compatibility')}] "
                      f"— {c.get('title')}")
    for w in warns:
        render.warn(w)
    for e in errors:
        render.fail(e)
    print()
    if errors:
        render.fail(f"gov contracts: FAIL ({len(errors)})")
        return 1
    render.ok(f"gov contracts: PASS ({active} active, {declared} declared)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
